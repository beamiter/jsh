//! Bash parity for arithmetic and brace expansion: `$((...))`, `((...))`,
//! `for ((...))`, `let`, array subscripts, substring bounds and `{...}`.
//!
//! Every expected stdout and exit status below was produced by bash 5.2
//! (`bash --norc --noprofile -c SCRIPT`); bash itself is never run here.
use std::process::{Command, Stdio};

/// Run `script` with jsh in an empty temporary directory. The script goes
/// through a file, since some cases exceed the kernel's per-argument limit.
fn run(script: &str) -> (String, String, i32) {
    let dir = tempfile::tempdir().expect("temp dir");
    let path = dir.path().join("script.sh");
    std::fs::write(&path, script).expect("write script");
    let out = Command::new(env!("CARGO_BIN_EXE_jsh"))
        .arg("--norc")
        .arg(&path)
        .current_dir(dir.path())
        .stdin(Stdio::null())
        .output()
        .expect("spawn jsh");
    (
        String::from_utf8_lossy(&out.stdout).into_owned(),
        String::from_utf8_lossy(&out.stderr).into_owned(),
        out.status.code().unwrap_or(-1),
    )
}

/// Check every `(script, stdout, status)` case, reporting all mismatches at
/// once. These scripts succeed in bash, so jsh must not complain either.
fn assert_cases(cases: &[(&str, &str, i32)]) {
    let mut failures = Vec::new();
    for &(script, stdout, status) in cases {
        let (out, err, code) = run(script);
        if out != stdout || code != status || !err.is_empty() {
            failures.push(format!(
                "{script:?}\n  expected {stdout:?} (status {status})\n  got      {out:?} (status {code}), stderr {err:?}"
            ));
        }
    }
    assert!(failures.is_empty(), "{}", failures.join("\n"));
}

/// `((...))` text is read from the source, so `<=`, `>=`, `<<=` and `>>=`
/// never reach the shell lexer as redirections or here-docs.
#[test]
fn arithmetic_command_keeps_relational_and_shift_operators() {
    assert_cases(&[
        ("i=5; ((i>=3)) && echo ge; ((i<=3)) || echo notle; ((i<10 && i>2)) && echo range", "ge\nnotle\nrange\n", 0),
        ("x=1; ((x<<=3)); echo $x; ((x>>=1)); echo $x", "8\n4\n", 0),
        ("i=0; until ((i>=3)); do ((i++)); done; echo $i", "3\n", 0),
        ("fact() { (( $1 <= 1 )) && echo 1 && return; echo $(( $1 * $(fact $(( $1 - 1 ))) )); }; fact 5", "120\n", 0),
        ("((\n  x = 3 + 4\n)); echo $x", "7\n", 0),
        ("((x = 1 <<\n2)); echo $x", "4\n", 0),
        ("((x = 5)) > /dev/null; echo $x", "5\n", 0),
        ("((0)); echo $?; ((5)); echo $?; ((-1)); echo $?; (( )); echo $?", "1\n0\n0\n1\n", 0),
        ("f() ((x++)); x=1; f; echo $x", "2\n", 0),
    ]);
}

/// `for ((init; cond; update))` clauses come from the source text too and are
/// expanded afresh on every evaluation.
#[test]
fn arithmetic_for_headers_are_read_verbatim() {
    assert_cases(&[
        (
            "for ((i=1; i<=3; i++)); do echo -n $i; done; echo",
            "123\n",
            0,
        ),
        (
            "for ((i=3; i>=1; i--)); do echo -n $i; done; echo",
            "321\n",
            0,
        ),
        (
            r#"for ((i=1; i<=64; i<<=1)); do echo -n "$i "; done; echo"#,
            "1 2 4 8 16 32 64 \n",
            0,
        ),
        (
            "for ((i=0, j=10; i<j; i+=3, j-=3)); do echo $i $j; done",
            "0 10\n3 7\n",
            0,
        ),
        (
            "n=0; for ((;;)); do ((n++)); ((n>=3)) && break; done; echo $n",
            "3\n",
            0,
        ),
        (
            "for (( i = 0 ;\n       i < 2 ;\n       i++ )); do echo $i; done",
            "0\n1\n",
            0,
        ),
        (
            "arr=(5 6 7); for ((i=0;i<${#arr[@]};i++)); do echo -n ${arr[i]}; done; echo",
            "567\n",
            0,
        ),
        (
            "f() { for ((i=$1; i<=$2; i++)); do echo -n $i; done; echo; }; f 2 5",
            "2345\n",
            0,
        ),
        (
            "e=; n=0; for ((;$e;)); do ((n++)); ((n>2)) && break; done; echo $n",
            "0\n",
            0,
        ),
        ("for ((i=0;i<1;)); do ((i++)); done; echo $?", "1\n", 0),
    ]);
}

/// Bash reads `((` as two subshells when the inner `(` is not closed by `))`,
/// and `( (` with a space is always two subshells.
#[test]
fn double_parentheses_that_do_not_close_together_are_subshells() {
    assert_cases(&[
        ("((echo hi) | cat)", "hi\n", 0),
        ("( (echo hi) )", "hi\n", 0),
        ("((echo a); (echo b))", "a\nb\n", 0),
        ("(( (1+2) * 3 )); echo $?", "0\n", 0),
    ]);
}

/// Parameters, command substitutions and nested arithmetic expand first, as
/// inside double quotes; `$x` splices text while a bare `x` is a sub-expression.
#[test]
fn arithmetic_text_is_expanded_before_evaluation() {
    assert_cases(&[
        (
            "set -- 4 5; echo $(( $1 + $2 )) $(( $# * 10 ))",
            "9 20\n",
            0,
        ),
        ("s=hello; echo $(( ${#s} * 2 ))", "10\n", 0),
        ("echo $(( $(echo 3) + 4 ))", "7\n", 0),
        ("echo $(( $((1+2)) * 3 ))", "9\n", 0),
        (r#"echo $(( "1" + 2 )) $(( "1 + 2" * 3 ))"#, "3 7\n", 0),
        ("x='1+2'; echo $((x*2)) $(($x*2))", "6 5\n", 0),
        ("unset u; echo $(( ${u:-3} + 1 ))", "4\n", 0),
        ("echo $(( $(printf '%s' '1+') 2 ))", "3\n", 0),
        (r#"echo "$(( "1" + 2 ))""#, "3\n", 0),
        ("n=3; (( $(echo 2) * n == 6 )) && echo yes", "yes\n", 0),
    ]);
}

/// `**` is right-associative and binds looser than unary minus; compound
/// assignments cover every binary operator; constants accept `base#digits`.
#[test]
fn power_compound_assignment_and_bases_follow_bash() {
    assert_cases(&[
        ("echo $((2**10)) $((2**3**2)) $((-2**2)) $((2**0)) $((0**0))", "1024 512 4 1 1\n", 0),
        ("echo $((2**63)) $((2**64)) $((3**40))", "-9223372036854775808 0 -6289078614652622815\n", 0),
        ("x=3; echo $(( x ** 2 )) $(( 2 ** x ** 2 )) $(( 2 * 3 ** 2 ))", "9 512 18\n", 0),
        ("x=10; ((x<<=2, x|=1)); echo $x", "41\n", 0),
        ("x=5; ((x<<=1)); a=$x; ((x>>=2)); b=$x; ((x&=3)); c=$x; ((x^=7)); d=$x; ((x|=8)); e=$x; echo $a $b $c $d $e", "10 2 2 5 13\n", 0),
        ("echo $((0x1F)) $((8#17)) $((2#101)) $((017)) $((36#z))", "31 15 5 15 35\n", 0),
        ("echo $((64#_)) $((64#@)) $((64#Z)) $((64#z)) $((16#ff)) $((16#FF)) $((10#010))", "63 62 61 35 255 255 10\n", 0),
        ("echo $((2#1111)) $((8#777)) $((16#dead)) $((36#ZZ)) $((62#Z)) $((64#10)) $((0XfF)) $((0x))", "15 511 57005 1295 61 64 255 0\n", 0),
        ("x=010; echo $((x)) $((x+1))", "8 9\n", 0),
        ("echo $((--5)) $((++5)) $((1--1)) $((1---1)) $(( - -1 ))", "5 5 2 0 1\n", 0),
        ("a=1; echo $((a+++1)) $a; b=5; echo $((b---1)) $b", "2 2\n4 4\n", 0),
        ("echo $((!!5)) $((~~5)) $((-~5)) $((~-5)) $((1 << -1)) $((8 >> -1))", "1 5 6 4 -9223372036854775808 0\n", 0),
        ("echo $((-7 % 3)) $((7 % -3)) $((-7 / 3)) $((a=b=c=4)) $a $b $c", "-1 1 -2 4 4 4 4\n", 0),
    ]);
}

/// A value that is not an integer constant is evaluated recursively, with
/// bash's recursion limit.
#[test]
fn variable_values_are_evaluated_as_expressions() {
    assert_cases(&[
        ("a=b; b=3; echo $((a+1))", "4\n", 0),
        ("x=y; y=z; z=7; echo $((x)) $((x*x))", "7 49\n", 0),
        ("a='b=5'; echo $((a)) $b", "5 5\n", 0),
        (
            "unset q; e=; s=' 42 '; n=-3; echo $((q+1)) $((e+1)) $((s+1)) $((n*2)) $((-n))",
            "1 1 43 -6 3\n",
            0,
        ),
        (r#"x=" 1 + 2 "; echo $((x))"#, "3\n", 0),
        (r#"b=7; a="b"; echo $(( a++ )) $a $b"#, "7 8 7\n", 0),
        (r#"a="1+1"; echo $(( a++ )) $a"#, "2 3\n", 0),
        (r#"x="3 "; echo $((++x))"#, "4\n", 0),
        ("x=1; (( x += x++ )); echo $x", "2\n", 0),
    ]);
}

/// Indexed subscripts are arithmetic; associative subscripts are key text.
#[test]
fn array_elements_are_operands_and_lvalues() {
    assert_cases(&[
        ("arr=(1 2 3); echo $((arr[1]+arr[2]))", "5\n", 0),
        ("arr=(1 2 3); ((arr[0]+=10)); echo ${arr[0]}", "11\n", 0),
        (
            "arr=(10 20 30); i=1; echo $((arr[i+1])) $((arr[i]++)) ${arr[1]}",
            "30 20 21\n",
            0,
        ),
        (
            "declare -A m; m[k]=5; echo $((m[k]*2)); ((m[k]++)); echo ${m[k]}",
            "10\n6\n",
            0,
        ),
        (
            "counts=(0 0 0); for i in 0 1 1 2 2 2; do ((counts[i]++)); done; echo ${counts[@]}",
            "1 2 3\n",
            0,
        ),
        ("a=(1 2 3); echo $((a)) $((a[0])) $((a[5]))", "1 1 0\n", 0),
        ("a=(1 2 3); i=1; echo $((a[i]+a[$i]))", "4\n", 0),
        (
            "declare -A m=([a]=5 [b]=6); k=b; echo $((m[a]+m[$k]+m[b]))",
            "17\n",
            0,
        ),
        (
            r#"declare -A m; m[k]=1; (( m[ k ]++ )); echo ${m[k]} ${m[" k "]}"#,
            "1 1\n",
            0,
        ),
        ("x=5; echo $((x[0])) $((x[1]))", "5 0\n", 0),
        (
            "i=0; a=(5 6); echo $(( a[i++] += 10 )) $i ${a[*]}",
            "15 1 15 6\n",
            0,
        ),
        ("a=(1 2 3); ((a=9)); echo ${a[@]}", "9 2 3\n", 0),
        ("arr=(10 20); echo $(( arr[ 1 ] ))", "20\n", 0),
        ("x=0; a=(1 2); echo $((0 && a[x++])) $x", "0 0\n", 0),
    ]);
}

/// `${a[expr]}`, `a[expr]=v` and `${s:off:len}` evaluate arithmetically.
#[test]
fn subscripts_and_substring_bounds_outside_arithmetic() {
    assert_cases(&[
        (
            "arr=(a b c d); echo ${arr[1+2]} ${arr[$((1+1))]}",
            "d c\n",
            0,
        ),
        ("s=hello; echo ${s:1+1:2} ${s:$((1+1)):2}", "ll ll\n", 0),
        (
            "s=abcdef; o=2; l=3; echo ${s:1+1} ${s: -2} ${s:(-3):2} ${s:0:2*2} ${s:o:l}",
            "cdef ef de abcd cde\n",
            0,
        ),
        ("a=(aa bbb c); i=1; echo ${#a[i]} ${#a[i+1]}", "3 1\n", 0),
        (
            "declare -A m; m[a]=1; k=a; echo ${m[$k]}; m[$k]=2; echo ${m[a]}",
            "1\n2\n",
            0,
        ),
        ("a=(x y z); i=1; a[i+1]=Q; echo ${a[@]}", "x y Q\n", 0),
        ("arr=(); arr[2+3]=x; echo ${arr[5]}", "x\n", 0),
        ("declare -A m; m[1+2]=x; echo ${m[1+2]} ${m[3]}", "x\n", 0),
        ("a=(x y z); echo ${a[i=1]} $i", "y 1\n", 0),
        ("i=0; a[i+1]=$((i=7)); echo ${a[8]} $i", "7 7\n", 0),
    ]);
}

/// Bash `let`: status 0 when the last value is non-zero.
#[test]
fn let_evaluates_each_argument() {
    assert_cases(&[
        ("let x=5*2 y=x+1; echo $x $y", "10 11\n", 0),
        (
            r#"let 0; echo $?; let 1; echo $?; let "a = 0" "b = 3"; echo $? $a $b"#,
            "1\n0\n0 0 3\n",
            0,
        ),
        ("i=1; let i++ i+=10; echo $i", "12\n", 0),
        (r#"let "v = 2 * (3 + 4)"; echo $v"#, "14\n", 0),
        ("let -- x=3; echo $? $x", "0 3\n", 0),
    ]);
}

/// Alternatives split only on top-level commas and may nest; sequences pad
/// and step like bash; anything else stays literal.
#[test]
fn brace_expansion_nests_and_respects_quoting() {
    assert_cases(&[
        ("echo {a,{b,c}}", "a b c\n", 0),
        ("echo x{a,b{1,2},c}y", "xay xb1y xb2y xcy\n", 0),
        ("echo {a,{1..3}}", "a 1 2 3\n", 0),
        (r#"echo {a,'b,c'} {a,b\,c} {"a,b",c} {a,"b"}"#, "a b,c a b,c a,b c a b\n", 0),
        ("echo a{,b} a{,,}b", "a ab ab ab ab\n", 0),
        ("printf '[%s]' {,} x {,a}; echo", "[x][a]\n", 0),
        ("echo {08..11} {-2..2} {-05..3} {001..3} {1..003} {-1..02}", "08 09 10 11 -2 -1 0 1 2 -05 -04 -03 -02 -01 000 001 002 003 001 002 003 001 002 003 -1 00 01 02\n", 0),
        ("echo {a..j..3} {a..e..2} {z..a..10} {1..10..3} {10..1..-3} {1..3..0}", "a d g j a c e z p f 1 4 7 10 10 7 4 1 1 2 3\n", 0),
        ("echo {1..x} {a..5} {1..3..x} {..3} {1...3} {1..2..3..4} {1..99999999999999999999}", "{1..x} {a..5} {1..3..x} {..3} {1...3} {1..2..3..4} {1..99999999999999999999}\n", 0),
        ("x=1; echo {a} {'a'} {$x} {$x,b} {}", "{a} {a} {1} 1 b {}\n", 0),
        ("echo {a{b,c}} {'1'..3}", "{ab} {ac} {1..3}\n", 0),
        ("x=1; echo {a,$(echo b,c)} {a,${x:-b,c}}", "a b,c a 1\n", 0),
        ("echo {a,b}{1,2}", "a1 a2 b1 b2\n", 0),
        ("echo {a,b x{a,b}{ }{a,b}", "{a,b xa{ xb{ }a }b\n", 0),
    ]);
}

/// Failures keep bash's stdout and status: evaluation errors in `((...))`,
/// `for ((...))` and `let` return 1 and the script continues, while errors
/// while expanding a word (`$((...))`, `${s:off}`) abort it. Each one reports
/// a `jsh:` diagnostic.
#[test]
fn arithmetic_errors_match_bash_status_and_report_a_diagnostic() {
    let cases: &[(&str, &str, i32)] = &[
        ("echo $((1/0)); echo st=$?", "", 1),
        ("((1/0)); echo st=$?", "st=1\n", 0),
        ("echo $(( 1 @ 2 )); echo st=$?", "", 1),
        ("echo $((2**-1)); echo st=$?", "", 1),
        ("echo $((2#102)); echo st=$?", "", 1),
        ("echo $((65#1)); echo st=$?", "", 1),
        ("echo $((16#)); echo st=$?", "", 1),
        ("echo $((0#5)); echo st=$?", "", 1),
        ("echo $((08)); echo st=$?", "", 1),
        ("x=x; echo $((x)); echo st=$?", "", 1),
        ("x=08; echo $((x)); echo after", "", 1),
        ("echo a; echo $((1/0)); echo b", "a\n", 1),
        ("f() { echo $((1/0)); echo inf; }; f; echo after", "", 1),
        ("let 1/0; echo st=$?", "st=1\n", 0),
        ("let x=1/0 y=2; echo $? $y", "1\n", 0),
        ("let; echo st=$?", "st=1\n", 0),
        (r#"let "1 +"; echo st=$?; echo after"#, "st=1\nafter\n", 0),
        (
            "for ((i=0; i<1/0; i++)); do :; done; echo st=$?",
            "st=1\n",
            0,
        ),
        (
            "for ((i=0/0; i<1; i++)); do :; done; echo st=$?",
            "st=1\n",
            0,
        ),
        (
            "for ((i=0; i<$((1/0)); i++)); do :; done; echo st=$?",
            "",
            1,
        ),
        ("(( $((1/0)) )); echo st=$?", "", 1),
        (
            "set -e; for ((i=0; i<1/0; i++)); do :; done; echo after $?",
            "after 1\n",
            0,
        ),
        (
            "set -e; for ((i=0; i<2; i+=1/0)); do echo body; done; echo after $?",
            "body\nafter 1\n",
            0,
        ),
        (
            "set -e; f() { for ((i=0; i<1/0; )); do :; done; }; f; echo after $?",
            "",
            1,
        ),
        ("set -e; ((1/0)); echo after", "", 1),
        ("echo $((0 && 2**-1)); echo st=$?", "", 1),
        ("echo $((0 && 08)); echo st=$?", "", 1),
        ("echo $(( '1' + 2 )); echo st=$?", "", 1),
        (r#"x=\$y; y=3; echo $((x)); echo st=$?"#, "", 1),
        (r#"y=3; let "x=\$y"; echo $? $x"#, "1\n", 0),
        ("echo $(( (x) = 3 )); echo st=$?", "", 1),
        ("echo $(( a[1 )); echo st=$?", "", 1),
        (r#"x="1 2"; echo $((x)); echo st=$?"#, "", 1),
        ("echo $(( 1 ? : 2 )); echo st=$?", "", 1),
        ("x=5; ((x /= 0)); echo st=$? x=$x", "st=1 x=5\n", 0),
        ("s=abc; echo ${s:1/0}; echo after", "", 1),
    ];
    let mut failures = Vec::new();
    for &(script, stdout, status) in cases {
        let (out, err, code) = run(script);
        if out != stdout || code != status || !err.starts_with("jsh: ") {
            failures.push(format!(
                "{script:?}\n  expected {stdout:?} (status {status})\n  got      {out:?} (status {code}), stderr {err:?}"
            ));
        }
    }
    assert!(failures.is_empty(), "{}", failures.join("\n"));
}

#[test]
fn error_messages_name_the_failing_text() {
    for (script, fragment) in [
        ("echo $((1/0))", "1/0: division by 0"),
        ("((1/0))", "((: 1/0: division by 0"),
        ("let 1/0", "let: 1/0: division by 0"),
        ("echo $((2**-1))", "exponent less than 0"),
        ("echo $((2#102))", "2#102: value too great for base"),
        ("echo $((65#1))", "65#1: invalid arithmetic base"),
        ("echo $((1 @ 2))", "invalid arithmetic operator"),
        ("x=08; echo $((x))", "08: value too great for base"),
        ("x=x; echo $((x))", "x: expression recursion level exceeded"),
        ("set -u; echo $((q + 1))", "q: unbound variable"),
    ] {
        let (_, err, code) = run(script);
        assert_eq!(code, 1, "{script}: {err}");
        assert!(
            err.contains(fragment),
            "{script}: expected {fragment:?} in {err:?}"
        );
    }
}

/// `set -u` makes an unset variable in arithmetic fatal, whether it is spliced
/// in (`$q`) or named (`q`), in every arithmetic context. (Bash exits with 127
/// here; jsh reports unbound variables with status 1 everywhere.)
#[test]
fn unbound_variables_abort_under_set_u() {
    for script in [
        "set -u; echo $(( $q + 1 )); echo after",
        "set -u; (( $q + 1 )); echo after",
        "set -u; (( q + 1 )); echo after",
        "set -u; let q+1; echo after",
        "set -u; for ((i=0; i<n; i++)); do :; done; echo after",
        "set -u; set -- 1; echo $(( $1 + $2 )); echo after",
    ] {
        let (out, err, code) = run(script);
        assert_eq!((out.as_str(), code), ("", 1), "{script}: {err}");
        assert!(err.contains("unbound variable"), "{script}: {err}");
    }
    let (out, err, code) = run("set -u; a=(1); echo $((a[5])) $(( ${q:-2} + 1 ))");
    assert_eq!(
        (out.as_str(), code),
        (
            "0 3
", 0
        ),
        "{err}"
    );
}

/// Nesting is bounded: absurdly deep expressions fail with bash's recursion
/// error instead of overflowing the stack, while ordinary depth still works.
#[test]
fn deep_nesting_is_bounded_without_crashing() {
    let parens = |depth: usize| format!("{}1{}", "(".repeat(depth), ")".repeat(depth));
    let (out, err, code) = run(&format!("echo $(({}))", parens(500)));
    assert_eq!((out.as_str(), code), ("1\n", 0), "{err}");

    let (out, err, code) = run(&format!("(({})); echo st=$?", parens(100_000)));
    assert_eq!(out, "st=1\n");
    assert_eq!(code, 0);
    assert!(err.contains("expression recursion level exceeded"), "{err}");

    let chain: String = (0..500).map(|i| format!("v{i}=v{}; ", i + 1)).collect();
    let (out, err, code) = run(&format!("{chain}v500=42; echo $((v0))"));
    assert_eq!((out.as_str(), code), ("42\n", 0), "{err}");

    for expression in ["!".repeat(5000) + "1", "-".repeat(5000) + "1"] {
        let (out, err, code) = run(&format!("echo $(({expression})); echo st=$?"));
        assert_eq!((out.as_str(), code), ("", 1), "{err}");
        assert!(err.contains("expression recursion level exceeded"), "{err}");
    }
}
