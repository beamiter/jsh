//! Bash parity for the variable model: dynamic scoping, attributes, arrays,
//! the declaration builtins, parameter expansion and the special variables.
//!
//! Every expected output below was produced by bash 5.2 (`bash --norc
//! --noprofile -c SCRIPT`) in the same environment these tests give jsh: a
//! scratch working directory that is also `$HOME`, and nothing else in the
//! environment but PATH, LANG/LC_ALL and TERM. Bash is not run at test time.

use std::process::{Command, Stdio};

/// Run `script` with `jsh -c` and compare stdout and the exit status with
/// what bash produced for it.
#[track_caller]
fn check(script: &str, stdout: &str, status: i32) {
    let dir = tempfile::tempdir().expect("scratch directory");
    let out = Command::new(env!("CARGO_BIN_EXE_jsh"))
        .arg("--norc")
        .arg("-c")
        .arg(script)
        .current_dir(dir.path())
        .env_clear()
        .env("PATH", "/usr/local/bin:/usr/bin:/bin")
        .env("HOME", dir.path())
        .env("LANG", "C.UTF-8")
        .env("LC_ALL", "C.UTF-8")
        .env("TERM", "dumb")
        .stdin(Stdio::null())
        .output()
        .expect("spawn jsh");
    let actual = String::from_utf8_lossy(&out.stdout);
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert_eq!(
        actual, stdout,
        "stdout differs from bash for:\n{script}\nstderr:\n{stderr}"
    );
    assert_eq!(
        out.status.code(),
        Some(status),
        "exit status differs from bash for:\n{script}\nstderr:\n{stderr}"
    );
}

/// `SECONDS` counts whole seconds from its last assignment; a second boundary
/// may pass between the assignment and the read, so allow for one.
#[test]
fn seconds_counts_from_its_assignment() {
    let out = Command::new(env!("CARGO_BIN_EXE_jsh"))
        .args(["--norc", "-c", "SECONDS=100; echo $SECONDS"])
        .stdin(Stdio::null())
        .output()
        .expect("spawn jsh");
    let value: i64 = String::from_utf8_lossy(&out.stdout)
        .trim()
        .parse()
        .expect("SECONDS is a number");
    assert!((100..=101).contains(&value), "SECONDS={value}");
}

/// Pattern operators on long values answer promptly: a literal pattern is a
/// string search, and a bracket pattern of fixed width tries one length per
/// position instead of every substring.
#[test]
fn pattern_operators_scale_to_long_values() {
    check(
        r#"v=$(printf '%20000s' ''); a=${v// /_}; b=${v//[ ]/-}; c=${v%% *}; d=${v//x*/y}; echo ${#a} ${a:0:3} ${#b} ${b: -2} "[${#c}]" ${#d}"#,
        "20000 ___ 20000 -- [0] 20000\n",
        0,
    );
}

/// An assignment updates the nearest enclosing scope that has the name, else the global: `f() { x=1; }` sets a global.
#[test]
fn assignments_in_functions_follow_the_dynamic_scope_chain() {
    // scope_function_sets_new_global
    check(
        r#"f() { x=1; }; f; echo "[$x]""#,
        r#"[1]
"#,
        0,
    );
    // scope_dynamic_callee_sets_caller_local
    check(
        r#"f() { local y=1; g; echo "f sees [$y]"; }; g() { y=2; z=3; }; f; echo "[$y] [$z]""#,
        r#"f sees [2]
[] [3]
"#,
        0,
    );
    // scope_local_shadow_restored
    check(
        r#"x=g; f() { local x=l; x=changed; }; f; echo $x"#,
        r#"g
"#,
        0,
    );
    // scope_nested_local_visible_to_callee
    check(
        r#"f() { local v=fv; g; }; g() { echo "g sees $v"; }; f; echo "[$v]""#,
        r#"g sees fv
[]
"#,
        0,
    );
    // scope_local_then_global_assign_in_sub
    check(
        r#"f() { local a=1; ( a=2 ); echo $a; }; f"#,
        r#"1
"#,
        0,
    );
    // x_scope_nested_shadow
    check(
        r#"x=g; a() { local x=a; b; echo "a:$x"; }; b() { local x=b; c; echo "b:$x"; }; c() { x=c; }; a; echo "top:$x""#,
        r#"b:c
a:a
top:g
"#,
        0,
    );
    // x_scope_recursion_locals
    check(
        r#"f() { local n=$1; if ((n > 0)); then f $((n-1)); fi; echo "n=$n"; }; f 3"#,
        r#"n=0
n=1
n=2
n=3
"#,
        0,
    );
    // x_scope_return_via_global
    check(
        r#"get() { result="computed $1"; }; get x; echo "$result""#,
        r#"computed x
"#,
        0,
    );
    // y_counter_global
    check(
        r#"count=0; inc() { count=$((count+1)); }; for i in 1 2 3; do inc; done; echo $count"#,
        r#"3
"#,
        0,
    );
    // y_global_array_from_nested
    check(
        r#"outer() { inner; }; inner() { res=(a b); }; outer; echo ${res[1]}"#,
        r#"b
"#,
        0,
    );
    // x_scope_declare_in_function_local
    check(
        r#"f() { declare d=1; typeset t=2; }; f; echo "[$d][$t]""#,
        r#"[][]
"#,
        0,
    );
    // x_scope_command_sub_sees_locals
    check(
        r#"f() { local v=inner; echo "$(echo $v)"; }; f"#,
        r#"inner
"#,
        0,
    );
}

/// Builtins and loops that assign (`for`, `read`) set the visible variable, which is a global unless a local exists.
#[test]
fn loop_and_read_variables_are_assigned_in_the_dynamic_scope() {
    // scope_loop_var_in_function_global
    check(
        r#"f() { for i in 1 2 3; do :; done; }; f; echo $i"#,
        r#"3
"#,
        0,
    );
    // scope_read_in_function_global
    check(
        r#"f() { read -r line <<< "text"; }; f; echo $line"#,
        r#"text
"#,
        0,
    );
    // x_scope_loop_local
    check(
        r#"f() { local i; for i in a b; do :; done; echo "f:$i"; }; i=g; f; echo "top:$i""#,
        r#"f:b
top:g
"#,
        0,
    );
    // x_scope_read_local
    check(
        r#"f() { local v; read -r v <<< inner; echo "f:$v"; }; v=outer; f; echo "top:$v""#,
        r#"f:inner
top:outer
"#,
        0,
    );
    // y_while_read_append
    check(
        r#"lines=(); add() { lines+=("$1"); }; while read -r l; do add "$l"; done <<< $'a b\nc'; echo ${#lines[@]} "${lines[0]}""#,
        r#"2 a b
"#,
        0,
    );
    // y_eval_in_function
    check(
        r#"f() { eval "ex=1"; eval 'earr=(1 2 3)'; }; f; echo $ex ${#earr[@]}"#,
        r#"1 3
"#,
        0,
    );
}

/// `unset` of a caller's local removes it (revealing the global); of the function's own local leaves it declared but unset.
#[test]
fn unset_reveals_or_hides_according_to_scope() {
    // unset_local_reveals_global
    check(
        r#"x=g; f() { local x=l; unset x; echo "[$x]"; }; f"#,
        r#"[]
"#,
        0,
    );
    // x_scope_unset_in_callee_reveals_global
    check(
        r#"x=g; f() { local x=l; g; echo "f:[$x]"; }; g() { unset x; echo "g:[$x]"; }; f; echo "top:[$x]""#,
        r#"g:[g]
f:[g]
top:[g]
"#,
        0,
    );
    // x_scope_unset_own_local_then_assign
    check(
        r#"x=g; f() { local x=l; unset x; echo "1:[${x-U}]"; x=new; echo "2:[$x]"; }; f; echo "top:[$x]""#,
        r#"1:[U]
2:[new]
top:[g]
"#,
        0,
    );
    // local_unset_value
    check(
        r#"x=global; f() { local x; echo "[${x-unset}]"; }; f"#,
        r#"[unset]
"#,
        0,
    );
    // x_scope_unset_global_from_function
    check(
        r#"G=1; f() { unset G; }; f; echo "[${G-unset}]""#,
        r#"[unset]
"#,
        0,
    );
    // y_dynamic_unset_scope
    check(
        r#"x=global; a() { local x=a; b; echo "a:$x"; }; b() { unset x; x=fromb; }; a; echo "top:$x""#,
        r#"a:fromb
top:fromb
"#,
        0,
    );
}

/// `local -a`/`local -A`/`declare -A` in a function make locals; a plain `arr=(...)` sets the visible (usually global) array.
#[test]
fn arrays_are_scoped_like_scalars() {
    // array_local_scope
    check(
        r#"f() { local -a arr=(1 2); echo in ${#arr[@]}; }; arr=(x); f; echo out ${#arr[@]} ${arr[0]}"#,
        r#"in 2
out 1 x
"#,
        0,
    );
    // array_local_assoc
    check(
        r#"f() { local -A m=([k]=v); echo ${m[k]}; }; f; echo "[${m[k]}]""#,
        r#"v
[]
"#,
        0,
    );
    // array_in_function_global
    check(
        r#"f() { garr=(1 2 3); }; f; echo ${#garr[@]}"#,
        r#"3
"#,
        0,
    );
    // scope_array_set_in_function_global
    check(
        r#"f() { ga=(1 2); ga+=(3); }; f; echo ${ga[@]}"#,
        r#"1 2 3
"#,
        0,
    );
    // scope_assoc_declare_in_function_is_local
    check(
        r#"f() { declare -A m=([k]=v); }; f; echo "[${m[k]}]""#,
        r#"[]
"#,
        0,
    );
    // x_scope_local_array_restored
    check(
        r#"arr=(1 2 3); f() { local -a arr=(x); arr+=(y); echo "${arr[@]}"; }; f; echo "${arr[@]}""#,
        r#"x y
1 2 3
"#,
        0,
    );
    // x_scope_local_assoc_restored
    check(
        r#"declare -A m=([k]=g); f() { local -A m=([k]=l); echo ${m[k]}; }; f; echo ${m[k]}"#,
        r#"l
g
"#,
        0,
    );
    // y_local_sparse
    check(
        r#"f() { local -a arr; arr[3]=x; echo ${#arr[@]} ${!arr[@]}; }; f"#,
        r#"1 3
"#,
        0,
    );
    // y_funcname_scope_local_array_cleanup
    check(
        r#"f() { local -a tmp=(1 2); }; f; echo "[${tmp[@]}]" ${#tmp[@]}"#,
        r#"[] 0
"#,
        0,
    );
    // y_declare_gA
    check(
        r#"f() { declare -gA G=([k]=v); }; f; echo ${G[k]}"#,
        r#"v
"#,
        0,
    );
    // y_unset_global_elem_in_function
    check(
        r#"a=(1 2 3); f() { unset 'a[1]'; }; f; echo ${a[@]} ${!a[@]}"#,
        r#"1 3 0 2
"#,
        0,
    );
}

/// `NAME=value cmd` binds NAME for the command only, exported; a function can assign it without touching the caller's variable.
#[test]
fn prefix_assignments_make_a_temporary_scope() {
    // x_scope_prefix_function
    check(
        r#"f() { echo "f:$z"; z=changed; g; }; g() { echo "g:$z"; }; z=0; z=5 f; echo "top:$z""#,
        r#"f:5
g:changed
top:0
"#,
        0,
    );
    // x_scope_prefix_local_inherit
    check(
        r#"f() { local z; echo "[$z]"; }; z=5 f"#,
        r#"[5]
"#,
        0,
    );
    // x_scope_prefix_unset_reveals
    check(
        r#"g() { unset z; echo "[${z-U}]"; }; z=0; z=5 g"#,
        r#"[0]
"#,
        0,
    );
    // x_scope_prefix_builtin
    check(
        r#"IFS=: read -r a b c <<< "1:2:3"; echo "$a $b $c"; echo "[$IFS]" | od -c | head -1"#,
        r#"1 2 3
0000000   [      \t  \n   ]  \n
"#,
        0,
    );
    // x_scope_prefix_export_child
    check(
        r#"f() { sh -c 'echo child:$W'; }; W=7 f; echo "[$W]""#,
        r#"child:7
[]
"#,
        0,
    );
    // x_prefix_assign_function_restore_unset
    check(
        r#"f() { echo "in:$P"; }; P=1 f; echo "[${P-unset}]""#,
        r#"in:1
[unset]
"#,
        0,
    );
    // x_export_prefix_external
    check(
        r#"X=1 sh -c 'echo $X'; echo "[$X]""#,
        r#"1
[]
"#,
        0,
    );
    // x_export_prefix_overrides_exported
    check(
        r#"export E=1; E=2 sh -c 'echo $E'; echo $E"#,
        r#"2
1
"#,
        0,
    );
    // y_local_ifs
    check(
        r#"f() { local IFS=,; echo "$*"; }; f a b c; echo "[$IFS]" | od -An -c | head -1"#,
        r#"a,b,c
   [      \t  \n   ]  \n
"#,
        0,
    );
    // y_ifs_local_in_function_restored
    check(
        r#"f() { local IFS=:; set -- $1; echo $#; }; f "a:b:c"; s="x y"; set -- $s; echo $#"#,
        r#"3
2
"#,
        0,
    );
}

/// A plain assignment is a shell variable; `export`, `declare -x` and `local -x` put it in the environment of commands.
#[test]
fn only_exported_variables_reach_child_processes() {
    // x_export_unexported_not_in_env
    check(
        r#"x=1; sh -c 'echo "[$x]"'; export x; sh -c 'echo "[$x]"'"#,
        r#"[]
[1]
"#,
        0,
    );
    // x_export_later_assignment
    check(
        r#"export Y; Y=2; sh -c 'echo $Y'"#,
        r#"2
"#,
        0,
    );
    // declare_x
    check(
        r#"declare -x EXPORTED=yes; env | grep -c '^EXPORTED=yes'"#,
        r#"1
"#,
        0,
    );
    // declare_plus_x
    check(
        r#"export E=1; declare +x E; env | grep -c '^E=' ; echo $E"#,
        r#"0
1
"#,
        0,
    );
    // export_n
    check(
        r#"export E=1; export -n E; env | grep -c '^E=' ; echo $E"#,
        r#"0
1
"#,
        0,
    );
    // x_export_n_keeps_value
    check(
        r#"export Q=1; export -n Q; echo $Q; sh -c 'echo "[$Q]"'"#,
        r#"1
[]
"#,
        0,
    );
    // scope_export_in_function
    check(
        r#"f() { export FX=1; }; f; env | grep -c '^FX=1'"#,
        r#"1
"#,
        0,
    );
    // scope_local_export
    check(
        r#"f() { local -x LX=1; env | grep -c '^LX=1'; }; f; env | grep -c '^LX=' "#,
        r#"1
0
"#,
        1,
    );
    // x_export_local_x
    check(
        r#"f() { local -x LX=1; sh -c 'echo $LX'; }; f; sh -c 'echo "[$LX]"'"#,
        r#"1
[]
"#,
        0,
    );
    // x_scope_export_local
    check(
        r#"f() { local L=1; export L; sh -c 'echo c:$L'; }; f; sh -c 'echo after:[$L]'"#,
        r#"c:1
after:[]
"#,
        0,
    );
    // y_declare_x_in_function
    check(
        r#"f() { declare -x DX=1; sh -c 'echo c:$DX'; }; f; echo "[${DX-unset}]""#,
        r#"c:1
[unset]
"#,
        0,
    );
    // x_allexport
    check(
        r#"set -a; AE=1; sh -c 'echo $AE'"#,
        r#"1
"#,
        0,
    );
    // x_export_multiple
    check(
        r#"export A=1 B=2 C; env | grep -E '^(A|B|C)=' | /usr/bin/sort; declare -p C"#,
        r#"A=1
B=2
declare -x C
"#,
        0,
    );
    // y_export_array_no_env
    check(
        r#"export arr=(1 2) 2>/dev/null; env | grep -c '^arr=' ; echo ${arr[1]}"#,
        r#"0
2
"#,
        0,
    );
}

/// Assigning a readonly variable is an error that abandons the rest of the command line; builtins report status 1.
#[test]
fn readonly_variables_refuse_changes_like_bash() {
    // declare_r
    check(
        r#"declare -r ro=1; (ro=2) 2>/dev/null; echo st=$?"#,
        r#"st=1
"#,
        0,
    );
    // declare_r_message
    check(r#"declare -r ro=1; ro=2; echo after"#, r#""#, 1);
    // readonly_cmd
    check(r#"readonly R=1; R=2 2>/dev/null; echo $R"#, r#""#, 1);
    // readonly_unset
    check(
        r#"readonly R=1; unset R 2>/dev/null; echo st=$? $R"#,
        r#"st=1 1
"#,
        0,
    );
    // readonly_function_local
    check(
        r#"readonly G=1; f() { local G=2; }; f 2>/dev/null; echo st=$?"#,
        r#"st=1
"#,
        0,
    );
    // scope_readonly_in_function_aborts
    check(
        r#"declare -r ro=1; f() { ro=2; echo in-f; }; f; echo after"#,
        r#""#,
        1,
    );
    // scope_readonly_assignment_aborts_line
    check(r#"readonly R=1; R=2; echo "after $R""#, r#""#, 1);
    // x_scope_readonly_local
    check(
        r#"f() { local -r r=1; r=2; echo after; }; f; echo top"#,
        r#""#,
        1,
    );
    // x_scope_readonly_caller_local
    check(
        r#"f() { local -r r=1; g; }; g() { r=2; echo in-g; }; f; echo top"#,
        r#""#,
        1,
    );
    // x_readonly_name_only
    check(r#"X=5; readonly X; X=6; echo after"#, r#""#, 1);
    // x_array_readonly
    check(r#"readonly -a ra=(1 2); ra[0]=x; echo after"#, r#""#, 1);
    // x_array_readonly_append
    check(r#"a=(1); readonly a; a+=(2); echo after"#, r#""#, 1);
    // x_declare_readonly_errors
    check(
        r#"declare -r R=1; declare R=2; echo st=$?; declare +r R; echo st=$?; R=3; echo after"#,
        r#"st=1
st=1
"#,
        1,
    );
    // y_readonly_in_subshell_status
    check(
        r#"readonly R=1; (R=2); echo st=$?"#,
        r#"st=1
"#,
        0,
    );
    // y_unset_readonly_array_elem
    check(
        r#"readonly -a ra=(1 2); unset 'ra[0]'; echo st=$? ${ra[@]}"#,
        r#"st=1 1 2
"#,
        0,
    );
    // y_declare_i_readonly_combo
    check(
        r#"declare -ir IR=3*3; echo $IR; IR=1; echo after"#,
        r#"9
"#,
        1,
    );
    // x_readonly_prefix_command
    check(
        r#"readonly R=1; R=2 echo ran; echo st=$?"#,
        r#"ran
st=0
"#,
        0,
    );
    // uid_readonly
    check(r#"UID=5 2>/dev/null; echo st=$?"#, r#""#, 1);
    // y_readonly_local_scope_end
    check(
        r#"f() { local -r q=1; }; f; q=2; echo $q"#,
        r#"2
"#,
        0,
    );
}

/// `-i` evaluates arithmetic (also for `+=`), `-l`/`-u`/`-c` convert case.
#[test]
fn declare_attributes_transform_assigned_values() {
    // declare_i
    check(
        r#"declare -i n; n=1+2; echo $n; n+=5; echo $n"#,
        r#"3
8
"#,
        0,
    );
    // declare_i_invalid
    check(
        r#"declare -i n; n=abc; echo $n"#,
        r#"0
"#,
        0,
    );
    // declare_l_u
    check(
        r#"declare -l lo=HeLLo; declare -u up=HeLLo; echo $lo $up; lo=ABC; echo $lo"#,
        r#"hello HELLO
abc
"#,
        0,
    );
    // declare_c
    check(
        r#"declare -c cap=hELLO; echo $cap"#,
        r#"Hello
"#,
        0,
    );
    // x_declare_i_expr
    check(
        r#"typeset -i x=5*2; echo $x; x+=2*3; echo $x; x="x+1"; echo $x"#,
        r#"10
16
17
"#,
        0,
    );
    // x_declare_u_append
    check(
        r#"declare -u u=ab; u+=cd; echo $u; declare -c c=abc; c+=DEF; echo $c"#,
        r#"ABCD
Abcdef
"#,
        0,
    );
    // x_declare_plus_i
    check(
        r#"declare -i n=5; declare +i n; n=1+1; echo $n"#,
        r#"1+1
"#,
        0,
    );
    // x_integer_array
    check(
        r#"declare -ai ia=(1+1 2*3); echo ${ia[@]}; ia+=(4+4); echo ${ia[@]}"#,
        r#"2 6
2 6 8
"#,
        0,
    );
    // x_lowercase_array
    check(
        r#"declare -l -a la=(AbC DEF); echo ${la[@]}"#,
        r#"abc def
"#,
        0,
    );
    // y_append_scalar
    check(
        r#"x=5; x+=3; echo $x; declare -i i=0; i+=1; i+=1; echo $i"#,
        r#"53
2
"#,
        0,
    );
}

/// `declare -p` quoting, attribute letters and array layout are byte-for-byte bash's.
#[test]
fn declare_print_matches_bash_format() {
    // declare_p_scalar
    check(
        r#"x=1; declare -p x"#,
        r#"declare -- x="1"
"#,
        0,
    );
    // declare_p_array
    check(
        r#"arr=(a 'b c'); declare -p arr"#,
        r#"declare -a arr=([0]="a" [1]="b c")
"#,
        0,
    );
    // declare_p_assoc_single
    check(
        r#"declare -A m=([k]=v); declare -p m"#,
        r#"declare -A m=([k]="v" )
"#,
        0,
    );
    // declare_p_attrs
    check(
        r#"declare -ix n=5; declare -p n"#,
        r#"declare -ix n="5"
"#,
        0,
    );
    // declare_p_missing
    check(
        r#"declare -p nosuchvar; echo st=$?"#,
        r#"st=1
"#,
        0,
    );
    // x_declare_p_unset_decl
    check(
        r#"declare -i n; declare -p n; declare -a a; declare -p a; declare -A m; declare -p m"#,
        r#"declare -i n
declare -a a
declare -A m
"#,
        0,
    );
    // x_declare_p_empty_arrays
    check(
        r#"declare -a a=(); declare -A m=(); declare -p a m"#,
        r#"declare -a a=()
declare -A m=()
"#,
        0,
    );
    // x_declare_p_local
    check(
        r#"f() { local -i n=3; local s="x y"; declare -p n s; }; f"#,
        r#"declare -i n="3"
declare -- s="x y"
"#,
        0,
    );
    // x_declare_p_quoting
    check(
        r#"v='a"b$c`d\e'; declare -p v; w=$'tab\there'; declare -p w"#,
        r#"declare -- v="a\"b\$c\`d\\e"
declare -- w=$'tab\there'
"#,
        0,
    );
    // x_array_declare_p_after_ops
    check(
        r#"a=(x y z); unset 'a[1]'; a[5]=q; a+=(r); declare -p a"#,
        r#"declare -a a=([0]="x" [2]="z" [5]="q" [6]="r")
"#,
        0,
    );
    // x_array_scalar_to_array
    check(
        r#"s=hello; s[2]=x; declare -p s"#,
        r#"declare -a s=([0]="hello" [2]="x")
"#,
        0,
    );
    // x_array_assoc_declare_p_many
    check(
        r#"declare -A m=([one]=1 [two]=2 [three]=3); declare -p m"#,
        r#"declare -A m=([two]="2" [three]="3" [one]="1" )
"#,
        0,
    );
    // x_array_assoc_special_keys
    check(
        r#"declare -A m; m["a b"]=1; m[*]=2; m["@"]=3; declare -p m"#,
        r#"declare -A m=(["*"]="2" ["@"]="3" ["a b"]="1" )
"#,
        0,
    );
    // x_declare_scalar_to_assoc
    check(
        r#"x=1; declare -A x; declare -p x"#,
        r#"declare -A x=([0]="1" )
"#,
        0,
    );
    // y_declare_p_newline
    check(
        r#"x=$'a\nb'; declare -p x; echo "${x@Q}""#,
        r#"declare -- x=$'a\nb'
$'a\nb'
"#,
        0,
    );
    // export_p_format
    check(
        r#"export ZZZ=1; export -p | grep ZZZ"#,
        r#"declare -x ZZZ="1"
"#,
        0,
    );
    // readonly_p
    check(
        r#"readonly R1=1; readonly -p | grep -c R1"#,
        r#"1
"#,
        0,
    );
    // x_readonly_p_format
    check(
        r#"readonly R1=1; readonly -a RA=(1); readonly -p | grep -E 'R1|RA'"#,
        r#"declare -r R1="1"
declare -ar RA=([0]="1")
"#,
        0,
    );
    // x_declare_x_grep
    check(
        r#"export ZZ1=1; declare -x | grep ZZ1; declare -px ZZ1"#,
        r#"declare -x ZZ1="1"
declare -x ZZ1="1"
"#,
        0,
    );
}

/// Option combinations, errors and fallbacks of declare/local/unset.
#[test]
fn declare_and_unset_option_handling() {
    // declare_g
    check(
        r#"f() { declare -g G=global; }; f; echo $G"#,
        r#"global
"#,
        0,
    );
    // x_scope_declare_g_shadowed
    check(
        r#"x=g; f() { local x=l; declare -g x=G; echo "f:$x"; }; f; echo "top:$x""#,
        r#"f:l
top:G
"#,
        0,
    );
    // x_declare_multiple
    check(
        r#"declare a=1 b=2 c; echo "$a $b [${c-unset}]""#,
        r#"1 2 [unset]
"#,
        0,
    );
    // x_declare_invalid_name
    check(
        r#"declare 1x=2; echo st=$?; declare a-b; echo st=$?"#,
        r#"st=1
st=1
"#,
        0,
    );
    // x_declare_convert_errors
    check(
        r#"declare -A m; declare -a m; echo st=$?; declare -a ix=(1); declare -A ix; echo st=$?"#,
        r#"st=1
st=1
"#,
        0,
    );
    // x_declare_f_listing
    check(
        r#"f() { :; }; g() { :; }; declare -F; declare -F f; declare -f nope; echo st=$?"#,
        r#"declare -f f
declare -f g
f
st=1
"#,
        0,
    );
    // local_outside_function
    check(
        r#"local x=1 2>/dev/null; echo st=$?"#,
        r#"st=1
"#,
        0,
    );
    // x_local_outside_error
    check(
        r#"local x=1; echo st=$?"#,
        r#"st=1
"#,
        0,
    );
    // unset_func
    check(
        r#"f() { :; }; unset -f f; type f 2>/dev/null || echo gone"#,
        r#"gone
"#,
        0,
    );
    // unset_var_then_func
    check(r#"f() { :; }; unset f; type -t f"#, r#""#, 1);
    // unset_v
    check(
        r#"x=1; unset -v x; echo "[$x]""#,
        r#"[]
"#,
        0,
    );
    // x_unset_function_fallback
    check(
        r#"f() { echo f; }; unset f; f 2>/dev/null || echo gone"#,
        r#"gone
"#,
        0,
    );
    // x_unset_v_keeps_function
    check(
        r#"f() { echo fn; }; unset -v f; f"#,
        r#"fn
"#,
        0,
    );
    // x_unset_multiple
    check(
        r#"a=1 b=2; unset a b; echo "[${a-u}][${b-u}]""#,
        r#"[u][u]
"#,
        0,
    );
    // x_unset_invalid
    check(
        r#"unset 'a-b'; echo st=$?; unset -v 'a-b'; echo st=$?"#,
        r#"st=0
st=1
"#,
        0,
    );
    // x_unset_assoc_key_spaces
    check(
        r#"declare -A m=(["a b"]=1 [c]=2); unset 'm[a b]'; echo ${#m[@]} ${!m[@]}"#,
        r#"1 c
"#,
        0,
    );
}

/// `declare -n`/`local -n` follow chains, assign through, and `unset -n` removes the reference itself.
#[test]
fn namerefs_resolve_to_their_target() {
    // declare_n
    check(
        r#"x=1; declare -n ref=x; ref=5; echo $x $ref"#,
        r#"5 5
"#,
        0,
    );
    // local_n
    check(
        r#"f() { local -n r=$1; r=changed; }; v=orig; f v; echo $v"#,
        r#"changed
"#,
        0,
    );
    // nameref_array
    check(
        r#"arr=(1 2); declare -n r=arr; r[2]=3; echo ${arr[@]} ${#r[@]}"#,
        r#"1 2 3 3
"#,
        0,
    );
    // x_declare_n_chain
    check(
        r#"a=1; declare -n r1=a; declare -n r2=r1; r2=9; echo $a $r1 $r2"#,
        r#"9 9 9
"#,
        0,
    );
    // x_declare_n_unset
    check(
        r#"x=5; declare -n r=x; unset r; echo "[${x-unset}]"; declare -p r"#,
        r#"[unset]
declare -n r="x"
"#,
        0,
    );
    // x_declare_n_unset_n
    check(
        r#"x=5; declare -n r=x; unset -n r; echo "[$x]"; declare -p r 2>/dev/null; echo st=$?"#,
        r#"[5]
st=1
"#,
        0,
    );
    // x_declare_n_indirect
    check(
        r#"x=5; declare -n r=x; echo ${!r}"#,
        r#"x
"#,
        0,
    );
    // x_declare_n_for_loop
    check(
        r#"a=1 b=2; declare -n v; for v in a b; do echo "$v"; done"#,
        r#"1
2
"#,
        0,
    );
    // x_declare_n_self
    check(
        r#"declare -n s=s; echo st=$?"#,
        r#"st=1
"#,
        0,
    );
    // x_scope_nameref_array_assign
    check(
        r#"f() { local -n ref=$1; ref=(1 2 3); }; f out; echo "${out[@]} ${#out[@]}""#,
        r#"1 2 3 3
"#,
        0,
    );
    // y_nameref_assoc
    check(
        r#"declare -A m; f() { local -n ref=$1; ref[key]=val; }; f m; echo ${m[key]}"#,
        r#"val
"#,
        0,
    );
    // y_nameref_to_local_of_caller
    check(
        r#"f() { local v=1; g v; echo $v; }; g() { local -n r=$1; r=2; }; f"#,
        r#"2
"#,
        0,
    );
}

/// Holes stay holes, appends go after the highest index, and negative subscripts count from the end.
#[test]
fn indexed_arrays_are_sparse_and_accept_negative_subscripts() {
    // array_unset_elem
    check(
        r#"arr=(a b c); unset 'arr[1]'; echo ${arr[@]} ${!arr[@]} ${#arr[@]}"#,
        r#"a c 0 2 2
"#,
        0,
    );
    // array_sparse
    check(
        r#"arr=([2]=x [5]=y); echo ${!arr[@]} ${arr[5]} ${#arr[@]}"#,
        r#"2 5 y 2
"#,
        0,
    );
    // array_sparse_append
    check(
        r#"arr=([5]=a); arr+=(b); echo ${!arr[@]}"#,
        r#"5 6
"#,
        0,
    );
    // array_neg_index
    check(
        r#"arr=(a b c); echo ${arr[-1]} ${arr[-2]}"#,
        r#"c b
"#,
        0,
    );
    // array_neg_assign
    check(
        r#"arr=(a b c); arr[-1]=Z; echo ${arr[@]}"#,
        r#"a b Z
"#,
        0,
    );
    // array_elem_append
    check(
        r#"arr=(a b); arr[1]+=x; echo ${arr[@]}"#,
        r#"a bx
"#,
        0,
    );
    // array_append_indexed
    check(
        r#"arr=(a); arr+=([5]=z); echo ${!arr[@]}"#,
        r#"0 5
"#,
        0,
    );
    // array_compound_assign_index
    check(
        r#"declare -a a=([0]=x [3]=y); echo ${#a[@]} ${a[3]}"#,
        r#"2 y
"#,
        0,
    );
    // x_array_neg_sparse
    check(
        r#"a=([2]=x [5]=y); echo ${a[-1]} "[${a[-2]}]" ${a[-4]}"#,
        r#"y [] x
"#,
        0,
    );
    // x_array_arith_index
    check(
        r#"i=2; a=(); a[i+1]=x; a[$((i*2))]=y; echo "${!a[@]}"; echo ${a[i+1]} ${a[$i+2]}"#,
        r#"3 4
x y
"#,
        0,
    );
    // x_array_length_after_unset
    check(
        r#"a=(1 2 3 4); unset 'a[0]' 'a[2]'; echo ${#a[@]} ${!a[@]}"#,
        r#"2 1 3
"#,
        0,
    );
    // x_array_unset_whole_and_at
    check(
        r#"a=(1 2); unset a; echo "[${a[@]}] ${#a[@]}"; b=(1 2); unset 'b[@]'; echo "[${b[@]}]""#,
        r#"[] 0
[]
"#,
        0,
    );
    // x_array_append_multi
    check(
        r#"a=(1); a+=(2 3); a+=("4 5"); printf '[%s]' "${a[@]}"; echo"#,
        r#"[1][2][3][4 5]
"#,
        0,
    );
    // y_declare_a_then_append
    check(
        r#"declare -a arr; arr+=(x); arr+=(y); echo ${#arr[@]} ${arr[1]}"#,
        r#"2 y
"#,
        0,
    );
    // x_array_iterate_indices
    check(
        r#"a=(p q r); for i in "${!a[@]}"; do echo "$i=${a[i]}"; done"#,
        r#"0=p
1=q
2=r
"#,
        0,
    );
    // x_array_keys_quoted
    check(
        r#"a=([3]=x [7]=y); for k in "${!a[@]}"; do echo "k=$k"; done; echo "${!a[*]}""#,
        r#"k=3
k=7
3 7
"#,
        0,
    );
}

/// Keys are listed in bash's hash-table order, and compound assignments accept quoted keys and key/value lists.
#[test]
fn associative_arrays_list_in_bash_hash_order() {
    // array_assoc
    check(
        r#"declare -A m; m[x]=1; m[y]=2; echo ${m[x]} ${m[y]} ${#m[@]}"#,
        r#"1 2 2
"#,
        0,
    );
    // array_assoc_literal
    check(
        r#"declare -A m=([k1]=v1 [k2]=v2); echo ${m[k2]}"#,
        r#"v2
"#,
        0,
    );
    // array_assoc_keys_sorted
    check(
        r#"declare -A m=([b]=2 [a]=1); for k in $(printf '%s\n' "${!m[@]}" | /usr/bin/sort); do echo $k=${m[$k]}; done"#,
        r#"a=1
b=2
"#,
        0,
    );
    // array_assoc_unset
    check(
        r#"declare -A m=([a]=1 [b]=2); unset 'm[a]'; echo ${#m[@]} ${m[b]}"#,
        r#"1 2
"#,
        0,
    );
    // array_assoc_key_spaces
    check(
        r#"declare -A m; m["a b"]=1; key="a b"; echo ${m[$key]}"#,
        r#"1
"#,
        0,
    );
    // array_assoc_append
    check(
        r#"declare -A m=([a]=1); m+=([b]=2); m[a]+=0; echo ${m[a]} ${m[b]}"#,
        r#"10 2
"#,
        0,
    );
    // x_array_assoc_order_many
    check(
        r#"declare -A m; for k in apple banana cherry date elderberry fig grape; do m[$k]=1; done; echo "${!m[@]}""#,
        r#"cherry grape elderberry apple fig date banana
"#,
        0,
    );
    // x_array_assoc_order_numeric
    check(
        r#"declare -A m=([10]=a [2]=b [33]=c [1]=d); echo "${!m[@]}" "${m[@]}""#,
        r#"33 2 1 10 c b d a
"#,
        0,
    );
    // x_array_assoc_kv_pairs
    check(
        r#"declare -A m=(k1 v1 k2 v2); echo ${m[k1]} ${m[k2]}"#,
        r#"v1 v2
"#,
        0,
    );
    // y_assoc_append_value
    check(
        r#"declare -A m=([k]=a); m[k]+=b; m[new]+=c; echo ${m[k]} ${m[new]}"#,
        r#"ab c
"#,
        0,
    );
    // y_assoc_iterate_sorted
    check(
        r#"declare -A m=([z]=1 [a]=2 [m]=3); for k in "${!m[@]}"; do echo "$k=${m[$k]}"; done | /usr/bin/sort"#,
        r#"a=2
m=3
z=1
"#,
        0,
    );
}

/// `"${a[@]}"` is one field per element (none for an empty array); operators apply per element.
#[test]
fn array_expansions_produce_fields_like_bash() {
    // array_slice
    check(
        r#"arr=(a b c d e); echo ${arr[@]:1:2} ${arr[@]: -2}"#,
        r#"b c d e
"#,
        0,
    );
    // array_slice_sparse
    check(
        r#"arr=([1]=a [5]=b [9]=c); echo ${arr[@]:2}"#,
        r#"b c
"#,
        0,
    );
    // array_pattern_each
    check(
        r#"arr=(a b c); echo "${arr[@]/#/x-}" "${arr[@]/%/-y}""#,
        r#"x-a x-b x-c a-y b-y c-y
"#,
        0,
    );
    // array_upper_each
    check(
        r#"arr=(ab cd); echo "${arr[@]^^}""#,
        r#"AB CD
"#,
        0,
    );
    // array_trim_each
    check(
        r#"arr=(a.txt b.txt); echo "${arr[@]%.txt}""#,
        r#"a b
"#,
        0,
    );
    // array_len_elem
    check(
        r#"arr=(abc de); echo ${#arr[0]} ${#arr[1]} ${#arr}"#,
        r#"3 2 3
"#,
        0,
    );
    // array_scalar_as_elem0
    check(
        r#"s=hello; echo ${s[0]} ${#s[@]}"#,
        r#"hello 1
"#,
        0,
    );
    // x_array_empty_quoted_no_args
    check(
        r#"a=(); set -- "${a[@]}"; echo $#; set -- "${a[*]}"; echo $#; set -- "${u[@]}"; echo $#"#,
        r#"0
1
0
"#,
        0,
    );
    // x_array_star_ifs
    check(
        r#"a=(x y z); IFS=-; echo "${a[*]}"; echo "${a[@]}"; IFS=; echo "${a[*]}""#,
        r#"x-y-z
x y z
xyz
"#,
        0,
    );
    // x_array_copy
    check(
        r#"a=("a b" c); b=("${a[@]}"); echo ${#b[@]} "${b[0]}""#,
        r#"2 a b
"#,
        0,
    );
    // x_array_self_append
    check(
        r#"a=(1 2); a=("${a[@]}" 3); echo "${a[@]}""#,
        r#"1 2 3
"#,
        0,
    );
    // x_array_compound_split
    check(
        r#"v="a b"; a=($v "$v"); echo ${#a[@]}"#,
        r#"3
"#,
        0,
    );
    // x_array_assign_glob_element
    check(
        r#"touch f1 f2; a=(f*); echo ${#a[@]}; b=([0]=f*); echo "${b[0]}""#,
        r#"2
f*
"#,
        0,
    );
    // y_cmdsub_array
    check(
        r#"a=( $(echo "1 2" "3") ); echo ${#a[@]}; b=( "$(echo "1 2")" ); echo ${#b[@]}"#,
        r#"3
1
"#,
        0,
    );
    // y_quoted_default_array
    check(
        r#"a=(); echo "${a[@]:-x}" "${a[@]+set}"; b=(1); echo "${b[@]+set}""#,
        r#"x
set
"#,
        0,
    );
    // y_array_in_double_quotes_concat
    check(
        r#"a=(1 2); echo "x${a[@]}y"; set -- "x${a[@]}y"; echo $#"#,
        r#"x1 2y
2
"#,
        0,
    );
    // y_empty_string_elements
    check(
        r#"a=("" x ""); echo ${#a[@]}; set -- ${a[@]}; echo $#; set -- "${a[@]}"; echo $#"#,
        r#"3
1
3
"#,
        0,
    );
    // y_length_star
    check(
        r#"a=(1 2 3); echo ${#a[*]} ${#a}"#,
        r#"3 1
"#,
        0,
    );
    // y_star_slice_quoted
    check(
        r#"a=(p q r); IFS=,; echo "${a[*]:1}""#,
        r#"q,r
"#,
        0,
    );
    // x_param_array_suffix_each
    check(
        r#"a=(a b); echo "${a[@]/%/.bak}""#,
        r#"a.bak b.bak
"#,
        0,
    );
}

/// Pattern, replacement, case, substring and default operators.
#[test]
fn parameter_operators_match_bash() {
    // param_upper
    check(
        r#"v=hello; echo ${v^^} ${v^} ${v,,} ${v~} ${v~~}"#,
        r#"HELLO Hello hello Hello HELLO
"#,
        0,
    );
    // param_lower
    check(
        r#"v=HeLLo; echo ${v,,} ${v,}"#,
        r#"hello heLLo
"#,
        0,
    );
    // param_case_pattern
    check(
        r#"v=hello; echo ${v^^[lo]} ${v^[h]}"#,
        r#"heLLO Hello
"#,
        0,
    );
    // param_default_unset_vs_empty
    check(
        r#"unset u; e=; echo "[${u-d1}][${e-d2}][${u:-d3}][${e:-d4}]""#,
        r#"[d1][][d3][d4]
"#,
        0,
    );
    // param_assign_default
    check(
        r#"unset x; echo ${x:=dflt}; echo $x"#,
        r#"dflt
dflt
"#,
        0,
    );
    // param_alt
    check(
        r#"x=1; echo ${x:+alt}; unset x; echo "[${x:+alt}]""#,
        r#"alt
[]
"#,
        0,
    );
    // param_substr_neg
    check(
        r#"v=abcdef; echo ${v: -2} ${v:1:2} ${v: -3:2} ${v:2}"#,
        r#"ef bc de cdef
"#,
        0,
    );
    // param_substr_neglen
    check(
        r#"v=abcdef; echo ${v:1:-2}"#,
        r#"bcd
"#,
        0,
    );
    // string_index_arith
    check(
        r#"s=hello; echo ${s:$((1+1)):2}"#,
        r#"ll
"#,
        0,
    );
    // x_param_replace_forms
    check(
        r#"v=hello.world.txt; echo ${v/./-} ${v//./-} ${v/#hello/HI} ${v/%txt/md} ${v/o*o/X}"#,
        r#"hello-world.txt hello-world-txt HI.world.txt hello.world.md hellXrld.txt
"#,
        0,
    );
    // x_param_replace_class
    check(
        r#"v=education; echo ${v//[aeiou]/} ${v//[!aeiou]/_}"#,
        r#"dctn e_u_a_io_
"#,
        0,
    );
    // x_param_patsub_amp
    check(
        r#"v=abc; echo "${v/b/[&]}" "${v//?/<&>}" "${v/b/\&}""#,
        r#"a[b]c <a><b><c> a&c
"#,
        0,
    );
    // x_param_strip_forms
    check(
        r#"p=/usr/local/lib/libfoo.so.1; echo ${p#*/} ${p##*/} ${p%/*} ${p%%/*}X ${p%.*} ${p%%.*}"#,
        r#"usr/local/lib/libfoo.so.1 libfoo.so.1 /usr/local/lib X /usr/local/lib/libfoo.so /usr/local/lib/libfoo
"#,
        0,
    );
    // x_param_strip_quoted_pattern
    check(
        r#"v='*ab*'; echo "${v#"*"}" "${v%\*}" "${v#*}""#,
        r#"ab* *ab *ab*
"#,
        0,
    );
    // x_param_default_cmdsub
    check(
        r#"unset x; echo ${x:-$(echo computed)}"#,
        r#"computed
"#,
        0,
    );
    // x_param_assign_quoted
    check(
        r#"unset x; : "${x:="a b"}"; echo "[$x]""#,
        r#"[a b]
"#,
        0,
    );
    // x_param_case_unicode
    check(
        r#"v=ÉCOLE; echo ${v,,} ${v,}; w=éa; echo ${w^^} ${w^}"#,
        r#"école éCOLE
ÉA Éa
"#,
        0,
    );
    // x_param_toggle
    check(
        r#"v=HeLLo; echo ${v~~} ${v~}"#,
        r#"hEllO heLLo
"#,
        0,
    );
    // x_param_len_multibyte
    check(
        r#"v=日本語; echo ${#v}"#,
        r#"3
"#,
        0,
    );
    // x_param_nested_default
    check(
        r#"unset x y; echo ${x:-${y:-z}}"#,
        r#"z
"#,
        0,
    );
    // x_param_default_quoted_word
    check(
        r#"unset x; set -- ${x-"a b"} c; echo $#"#,
        r#"2
"#,
        0,
    );
    // x_param_alt_quoted
    check(
        r#"y="p q"; x=1; set -- "${x:+"$y"}"; echo $# "$1""#,
        r#"1 p q
"#,
        0,
    );
    // x_param_substr_arith
    check(
        r#"s=abcdef; echo ${s:1+1:2} ${s: -3:-1} ${s:0:${#s}-1} ${s:(-2)}"#,
        r#"cd de abcde ef
"#,
        0,
    );
    // x_param_substr_out_of_range
    check(
        r#"s=abc; echo "[${s:5}]" "[${s: -5}]" "[${s:1:10}]""#,
        r#"[] [] [bc]
"#,
        0,
    );
    // y_param_pattern_var_unquoted
    check(
        r#"p='*.txt'; f=a.txt; echo ${f%$p}X "${f%"$p"}""#,
        r#"aX a.txt
"#,
        0,
    );
    // y_param_replace_slash_in_var
    check(
        r#"v=a/b/c; echo ${v//\//_} ${v//"/"/-}"#,
        r#"a_b_c a-b-c
"#,
        0,
    );
    // y_param_replace_empty_rep
    check(
        r#"v=aXbXc; echo ${v//X} ${v/X}"#,
        r#"abc abXc
"#,
        0,
    );
}

/// `${@:2}`, `${!#}`, `${!prefix@}`, indirection and `$10`.
#[test]
fn special_and_positional_parameter_forms() {
    // param_prefix_names
    check(
        r#"abc1=1; abc2=2; echo ${!abc*}"#,
        r#"abc1 abc2
"#,
        0,
    );
    // param_prefix_names_at
    check(
        r#"abc1=1; abc2=2; for n in "${!abc@}"; do echo $n; done"#,
        r#"abc1
abc2
"#,
        0,
    );
    // param_indirect
    check(
        r#"a=b; b=val; echo ${!a}"#,
        r#"val
"#,
        0,
    );
    // param_indirect_array
    check(
        r#"arr=(x y z); ref='arr[1]'; echo ${!ref}"#,
        r#"y
"#,
        0,
    );
    // param_positional_slice
    check(
        r#"set -- a b c d; echo ${@:2} ${@:2:2} ${@: -1} ${*:1:1}"#,
        r#"b c d b c d a
"#,
        0,
    );
    // param_positional_pattern
    check(
        r#"set -- apple banana; echo "${@/a/A}" "${@^}""#,
        r#"Apple bAnana Apple Banana
"#,
        0,
    );
    // x_param_hash_forms
    check(
        r#"set -- a b c; echo ${#} ${##} ${#@} ${#*}"#,
        r#"3 1 3 3
"#,
        0,
    );
    // x_param_last_arg
    check(
        r#"set -- a b c; echo ${!#} ${@: -1} ${*: -1:1}"#,
        r#"c c c
"#,
        0,
    );
    // x_param_positional_quoted_slice
    check(
        r#"set -- "a b" "c d" e; printf '[%s]' "${@:2}"; echo; printf '[%s]' "${@:1:2}"; echo"#,
        r#"[c d][e]
[a b][c d]
"#,
        0,
    );
    // x_param_indirect_chain
    check(
        r#"a=b; b=c; c=val; echo ${!a} ${!b}"#,
        r#"c val
"#,
        0,
    );
    // x_param_indirect_positional
    check(
        r#"set -- x y z; n=2; echo ${!n}"#,
        r#"y
"#,
        0,
    );
    // x_param_star_ifs_join_assign
    check(
        r#"set -- a b c; IFS=,; x="$*"; y=$*; echo "$x" "$y""#,
        r#"a,b,c a,b,c
"#,
        0,
    );
    // x_param_at_in_assignment
    check(
        r#"set -- a b c; x=$@; echo "$x""#,
        r#"a b c
"#,
        0,
    );
    // y_positional_slice_short
    check(
        r#"set -- a; echo "[${@:2:1}]" "[${*:3}]""#,
        r#"[] []
"#,
        0,
    );
    // y_positional_ten
    check(
        r#"set -- 1 2 3 4 5 6 7 8 9 ten eleven; echo ${10} ${11} $10"#,
        r#"ten eleven 10
"#,
        0,
    );
    // y_shift_in_function
    check(
        r#"f() { shift; echo "$@"; }; set -- top args; f a b c; echo "$@""#,
        r#"b c
top args
"#,
        0,
    );
    // y_local_multi_default
    check(
        r#"f() { local x=$1 y=${2:-def}; echo "$x $y"; }; f one; f one two"#,
        r#"one def
one two
"#,
        0,
    );
}

/// `${v@Q}`, `@E`, `@A`, `@a`, `@K`, `@k`, `@U`, `@L`, `@u`.
#[test]
fn transformation_operators_match_bash() {
    // param_atQ
    check(
        r#"v="a b'c"; echo ${v@Q}"#,
        r#"'a b'\''c'
"#,
        0,
    );
    // param_atQ_array
    check(
        r#"arr=(a 'b c'); echo ${arr[@]@Q}"#,
        r#"'a' 'b c'
"#,
        0,
    );
    // param_atE
    check(
        r#"v='a\tb'; echo "${v@E}""#,
        r#"a	b
"#,
        0,
    );
    // param_atU
    check(
        r#"v=abc; echo ${v@U} ${v@L} ${v@u}"#,
        r#"ABC abc Abc
"#,
        0,
    );
    // param_atA
    check(
        r#"v=abc; echo ${v@A}"#,
        r#"v='abc'
"#,
        0,
    );
    // param_atA_array
    check(
        r#"arr=(x y); echo ${arr[@]@A}"#,
        r#"declare -a arr=([0]="x" [1]="y")
"#,
        0,
    );
    // param_ata
    check(
        r#"declare -i n=1; declare -a arr=(1); declare -A m=([k]=v); echo ${n@a} ${arr@a} ${m@a}"#,
        r#"i a A
"#,
        0,
    );
    // param_atK
    check(
        r#"declare -A m=([k]=v); echo ${m[@]@K}"#,
        r#"k "v"
"#,
        0,
    );
    // x_param_q_empty
    check(
        r#"e=; echo "[${e@Q}]" "[${nope@Q}]""#,
        r#"[''] []
"#,
        0,
    );
    // y_at_E
    check(
        r#"x='a\tb\x41'; printf '%s\n' "${x@E}""#,
        r#"a	bA
"#,
        0,
    );
}

/// RANDOM is bash's generator (same seed, same sequence), SECONDS counts from assignment, UID/EUID are readonly.
#[test]
fn dynamic_special_variables_behave_like_bash() {
    // random_seed_repeat
    check(
        r#"RANDOM=42; a=$RANDOM$RANDOM; RANDOM=42; b=$RANDOM$RANDOM; [[ $a == $b ]] && echo same"#,
        r#"same
"#,
        0,
    );
    // random_unset
    check(
        r#"unset RANDOM; RANDOM=5; echo $RANDOM"#,
        r#"5
"#,
        0,
    );
    // x_special_random_seed_exact
    check(
        r#"RANDOM=42; echo $RANDOM $RANDOM $RANDOM; RANDOM=1; echo $RANDOM"#,
        r#"17772 26794 1435
16807
"#,
        0,
    );
    // y_seconds_unset_normal
    check(
        r#"unset SECONDS; SECONDS=5; sleep 1; echo $SECONDS"#,
        r#"5
"#,
        0,
    );
    // y_random_local_shadow
    check(
        r#"f() { local RANDOM=5; echo $RANDOM; }; f"#,
        r#"5
"#,
        0,
    );
    // bashpid
    check(
        r#"[[ $BASHPID == $$ ]] && echo same; (x=$BASHPID; [[ $x != $$ ]] && echo sub-differs)"#,
        r#"same
sub-differs
"#,
        0,
    );
    // x_special_bashpid_cmdsub
    check(
        r#"a=$(echo $BASHPID); [[ $a != $$ ]] && echo differs"#,
        r#"differs
"#,
        0,
    );
    // x_special_euid_readonly
    check(r#"EUID=5 2>/dev/null; echo st=$?"#, r#""#, 1);
    // epochseconds
    check(
        r#"[[ $EPOCHSECONDS =~ ^[0-9]{10}$ ]] && echo ok"#,
        r#"ok
"#,
        0,
    );
    // epochrealtime
    check(
        r#"[[ $EPOCHREALTIME =~ ^[0-9]{10}\.[0-9]{6}$ ]] && echo ok"#,
        r#"ok
"#,
        0,
    );
    // srandom
    check(
        r#"[[ $SRANDOM =~ ^[0-9]+$ ]] && echo ok"#,
        r#"ok
"#,
        0,
    );
    // ostype
    check(
        r#"[[ -n $OSTYPE && -n $HOSTTYPE && -n $MACHTYPE ]] && echo ok"#,
        r#"ok
"#,
        0,
    );
    // shlvl
    check(
        r#"[[ $SHLVL =~ ^[0-9]+$ ]] && echo ok"#,
        r#"ok
"#,
        0,
    );
    // x_special_dollar_dash
    check(
        r#"set -u; echo $- | grep -c u"#,
        r#"1
"#,
        0,
    );
}

/// IFS whitespace and non-whitespace delimiters, and assignment values that are never split or globbed.
#[test]
fn word_splitting_follows_ifs() {
    // ifs_unset_default
    check(
        r#"unset IFS; x="a  b"; set -- $x; echo $#"#,
        r#"2
"#,
        0,
    );
    // ifs_join_star
    check(
        r#"arr=(a b c); IFS=,; echo "${arr[*]}""#,
        r#"a,b,c
"#,
        0,
    );
    // ifs_empty_no_split
    check(
        r#"IFS=; x="a b"; set -- $x; echo $#"#,
        r#"1
"#,
        0,
    );
    // x_ifs_custom_split
    check(
        r#"s="a,b,,c"; IFS=,; set -- $s; echo $#; for f; do echo "[$f]"; done"#,
        r#"4
[a]
[b]
[]
[c]
"#,
        0,
    );
    // x_ifs_trailing_nonws
    check(
        r#"s="a,b,"; IFS=,; set -- $s; echo $#"#,
        r#"2
"#,
        0,
    );
    // x_ifs_mixed_ws
    check(
        r#"s=" a , b "; IFS=" ,"; set -- $s; echo $#; printf '[%s]' "$@"; echo"#,
        r#"2
[a][b]
"#,
        0,
    );
    // x_ifs_newline
    check(
        r#"s=$'l1\nl2 x\nl3'; IFS=$'\n'; set -- $s; echo $#; echo "$2""#,
        r#"3
l2 x
"#,
        0,
    );
    // y_star_unquoted_ifs
    check(
        r#"a=(x y); IFS=:; echo ${a[*]}; b="1:2"; echo $b; set -- $b; echo $#"#,
        r#"x y
1 2
2
"#,
        0,
    );
    // y_ifs_whitespace_mix_tab
    check(
        r#"IFS=$' \t'; s=$'a\t\tb  c'; set -- $s; echo $#"#,
        r#"3
"#,
        0,
    );
    // y_ifs_default_restore
    check(
        r#"IFS=,; unset IFS; s="a b"; set -- $s; echo $#"#,
        r#"2
"#,
        0,
    );
    // y_assignment_word_split_not
    check(
        r#"v="a   b"; x=$v; echo "$x"; local_like() { local y=$v; echo "$y"; }; local_like; declare z=$v; echo "$z""#,
        r#"a   b
a   b
a   b
"#,
        0,
    );
    // y_declare_glob_no_expand
    check(
        r#"touch g1 g2; declare g=g*; echo "$g"; export E=g*; echo "$E""#,
        r#"g*
g*
"#,
        0,
    );
}

/// `~+`, `~-`, tildes in assignments, `nocaseglob`, and `case` patterns with quoting and `nocasematch`.
#[test]
fn tilde_and_matching_options() {
    // tilde_plus
    check(
        r#"cd /tmp; echo ~+"#,
        r#"/tmp
"#,
        0,
    );
    // tilde_minus
    check(
        r#"cd /tmp; cd /; echo ~-"#,
        r#"/tmp
"#,
        0,
    );
    // tilde_user
    check(
        r#"[[ ~root == /root ]] && echo root-home"#,
        r#"root-home
"#,
        0,
    );
    // x_tilde_assign
    check(
        r#"x=~/a; echo $x | sed "s|$HOME|H|"; y=a:~/b; echo $y | sed "s|$HOME|H|""#,
        r#"H/a
a:H/b
"#,
        0,
    );
    // x_tilde_declare
    check(
        r#"declare d=~/x; echo $d | sed "s|$HOME|H|"; f() { local l=~/y; echo $l | sed "s|$HOME|H|"; }; f"#,
        r#"H/x
H/y
"#,
        0,
    );
    // x_tilde_no_expand_quoted
    check(
        r#"echo "~" '~' ~x 2>/dev/null"#,
        r#"~ ~ ~x
"#,
        0,
    );
    // x_tilde_plus_minus_unset
    check(
        r#"unset OLDPWD; echo ~-"#,
        r#"~-
"#,
        0,
    );
    // y_tilde_in_array
    check(
        r#"a=(~ ~/x); echo ${a[@]} | sed "s|$HOME|H|g""#,
        r#"H H/x
"#,
        0,
    );
    // nocaseglob
    check(
        r#"touch ABC.txt; shopt -s nocaseglob; echo abc.*"#,
        r#"ABC.txt
"#,
        0,
    );
    // x_nocaseglob_multi
    check(
        r#"touch Abc.TXT abd.txt; shopt -s nocaseglob; echo a*.txt"#,
        r#"Abc.TXT abd.txt
"#,
        0,
    );
    // x_case_quoted_pattern
    check(
        r#"x='*'; case abc in "$x") echo literal;; *) echo glob;; esac; case '*' in "$x") echo lit2;; esac"#,
        r#"glob
lit2
"#,
        0,
    );
    // x_case_var_pattern
    check(
        r#"p='a*'; case abc in $p) echo matched;; esac"#,
        r#"matched
"#,
        0,
    );
    // y_case_nocasematch_bracket
    check(
        r#"shopt -s nocasematch; case ABC in [a-c]bc) echo m1;; esac; case X in "x") echo m2;; esac"#,
        r#"m1
m2
"#,
        0,
    );
    // y_case_escape
    check(
        r#"case 'a*' in a\*) echo esc;; esac; case 'ab' in a\*) echo wrong;; *) echo nomatch;; esac"#,
        r#"esc
nomatch
"#,
        0,
    );
}

/// Patterns common in real scripts: counters and accumulators in functions, `local IFS`, sourced libraries, `getopts`-free argument handling.
#[test]
fn real_script_idioms_behave_like_bash() {
    // z_readonly_in_function_global
    check(
        r#"f() { readonly RX=1; }; f; echo $RX; RX=2; echo after"#,
        r#"1
"#,
        1,
    );
    // z_unset_local_declare_p
    check(
        r#"f() { local x=1; unset x; declare -p x; echo st=$?; }; x=g; f"#,
        r#"declare -- x
st=0
"#,
        0,
    );
    // z_source_sets_global_from_function
    check(
        r#"printf 'sv=1\nsarr=(a b)\n' > s.sh; f() { . ./s.sh; }; f; echo $sv ${sarr[1]}"#,
        r#"1 b
"#,
        0,
    );
    // z_source_declare_in_function_local
    check(
        r#"printf 'declare -A sm=([k]=v)\n' > s.sh; f() { . ./s.sh; echo "in:${sm[k]}"; }; f; echo "out:[${sm[k]}]""#,
        r#"in:v
out:[]
"#,
        0,
    );
    // z_local_after_global_same_line
    check(
        r#"x=1; f() { local x=2; x=3; echo $x; }; f; echo $x"#,
        r#"3
1
"#,
        0,
    );
    // z_declare_i_in_loop
    check(
        r#"declare -i total=0; for n in 1 2 3; do total+=n*2; done; echo $total"#,
        r#"12
"#,
        0,
    );
    // z_assoc_in_function_global_modify
    check(
        r#"declare -A cfg=([a]=1); set_cfg() { cfg[$1]=$2; }; set_cfg b 2; echo ${#cfg[@]} ${cfg[b]}"#,
        r#"2 2
"#,
        0,
    );
    // z_nested_array_func_args
    check(
        r#"f() { local -a list=("$@"); echo ${#list[@]} "${list[1]}"; }; f "a b" c "d e""#,
        r#"3 c
"#,
        0,
    );
    // z_export_function_scope_restored_env
    check(
        r#"export V=outer; f() { local V=inner; sh -c 'echo c:$V'; }; f; sh -c 'echo c2:$V'"#,
        r#"c:inner
c2:outer
"#,
        0,
    );
    // z_unset_exported_removes_env
    check(
        r#"export U=1; unset U; sh -c 'echo "[$U]"'"#,
        r#"[]
"#,
        0,
    );
    // z_export_n_in_function_global
    check(
        r#"export N=1; f() { export -n N; }; f; sh -c 'echo "[$N]"'; echo $N"#,
        r#"[]
1
"#,
        0,
    );
    // z_temp_env_read_builtin_restores
    check(
        r#"x=orig; x=temp read y <<< "v"; echo $x $y"#,
        r#"orig v
"#,
        0,
    );
    // z_dollar_hash_in_function
    check(
        r#"f() { echo $#; }; f a b c; set -- x; f"#,
        r#"3
0
"#,
        0,
    );
    // y_local_ifs
    check(
        r#"f() { local IFS=,; echo "$*"; }; f a b c; echo "[$IFS]" | od -An -c | head -1"#,
        r#"a,b,c
   [      \t  \n   ]  \n
"#,
        0,
    );
    // y_eval_in_function
    check(
        r#"f() { eval "ex=1"; eval 'earr=(1 2 3)'; }; f; echo $ex ${#earr[@]}"#,
        r#"1 3
"#,
        0,
    );
    // y_nameref_to_local_of_caller
    check(
        r#"f() { local v=1; g v; echo $v; }; g() { local -n r=$1; r=2; }; f"#,
        r#"2
"#,
        0,
    );
}

/// Element namerefs, `local -I`, attribute-carrying associative arrays and compound re-assignment.
#[test]
fn subscripts_namerefs_and_attributes_combine_like_bash() {
    // w_indirect_default
    check(
        r#"unset t; ref=t; echo "${!ref:-dflt}"; t=set; echo "${!ref:-dflt}""#,
        r#"dflt
set
"#,
        0,
    );
    // w_nameref_keys
    check(
        r#"arr=([3]=a [9]=b); declare -n r=arr; echo ${!r[@]} ${#r[@]} ${r[-1]}"#,
        r#"3 9 2 b
"#,
        0,
    );
    // w_assoc_elem_length
    check(
        r#"declare -A m=([k]=hello); echo ${#m[k]} ${m[k]:1:2} ${m[k]^^}"#,
        r#"5 el HELLO
"#,
        0,
    );
    // w_nameref_elem
    check(
        r#"arr=(a b c); declare -n e='arr[1]'; echo $e; e=Z; echo ${arr[*]}"#,
        r#"b
a Z c
"#,
        0,
    );
    // w_declare_p_nameref_elem
    check(
        r#"arr=(a b); declare -n e='arr[1]'; declare -p e"#,
        r#"declare -n e="arr[1]"
"#,
        0,
    );
    // w_unset_nameref_elem
    check(
        r#"arr=(a b c); declare -n e='arr[1]'; unset e; echo ${!arr[@]}"#,
        r#"0 2
"#,
        0,
    );
    // w_local_inherit_I
    check(
        r#"x=outer; f() { local -I x; echo "[$x]"; }; f"#,
        r#"[outer]
"#,
        0,
    );
    // w_declare_g_array_in_function
    check(
        r#"f() { declare -ga GA=(1 2); }; f; echo ${GA[1]}"#,
        r#"2
"#,
        0,
    );
    // w_export_assoc
    check(
        r#"declare -A m=([a]=1); export m; env | grep -c '^m='; echo ${m[a]}"#,
        r#"0
1
"#,
        0,
    );
    // w_readonly_assoc_elem
    check(
        r#"declare -A m=([a]=1); readonly m; m[b]=2; echo after"#,
        r#""#,
        1,
    );
    // w_integer_assoc
    check(
        r#"declare -Ai m; m[x]=2*3; echo ${m[x]}"#,
        r#"6
"#,
        0,
    );
    // w_upper_assoc
    check(
        r#"declare -Au m; m[x]=abc; echo ${m[x]}"#,
        r#"ABC
"#,
        0,
    );
    // w_param_star_quoted_empty
    check(
        r#"set --; printf '[%s]' "$*"; echo; printf '[%s]' "$@"; echo"#,
        r#"[]
[]
"#,
        0,
    );
    // w_array_star_quoted_empty
    check(
        r#"a=(); printf '[%s]' "${a[*]}"; echo"#,
        r#"[]
"#,
        0,
    );
    // w_unset_middle_then_slice
    check(
        r#"a=(0 1 2 3 4); unset 'a[2]'; echo "${a[@]:1:3}""#,
        r#"1 3 4
"#,
        0,
    );
    // w_negative_index_unset
    check(
        r#"a=(1 2 3); unset 'a[-1]'; echo ${a[@]} ${#a[@]}"#,
        r#"1 2 2
"#,
        0,
    );
    // w_arr_assign_negative_out_of_range
    check(r#"a=(1); a[-3]=x; echo after"#, r#""#, 1);
    // w_compound_key_arith
    check(
        r#"i=1; a=([i+1]=x [i*5]=y); echo ${!a[@]}"#,
        r#"2 5
"#,
        0,
    );
    // w_assoc_compound_append_existing
    check(
        r#"declare -A m=([a]=1 [b]=2); m+=([a]=9); echo ${m[a]} ${m[b]} ${#m[@]}"#,
        r#"9 2 2
"#,
        0,
    );
    // w_compound_reassign_resets
    check(
        r#"a=(1 2 3); a=(x); echo ${#a[@]}"#,
        r#"1
"#,
        0,
    );
    // w_scalar_assign_to_array_elem0
    check(
        r#"a=(1 2 3); a=new; echo ${a[@]}"#,
        r#"new 2 3
"#,
        0,
    );
    // w_func_local_array_from_args
    check(
        r#"f() { local arr=("$@"); echo ${#arr[@]}; }; f a "b c" d"#,
        r#"3
"#,
        0,
    );
    // w_declare_without_value_keeps
    check(
        r#"x=5; declare x; echo $x; f() { local x; echo "[${x-unset}]"; }; f"#,
        r#"5
[unset]
"#,
        0,
    );
    // w_local_twice_same_scope
    check(
        r#"f() { local a=1; local a; echo $a; local a=2; echo $a; }; f"#,
        r#"1
2
"#,
        0,
    );
    // z_indirect_array_all
    check(
        r#"arr=(x y); ref='arr[@]'; echo "${!ref}""#,
        r#"x y
"#,
        0,
    );
    // z_param_length_array_elem_unset
    check(
        r#"a=(1); echo "[${#a[5]}]""#,
        r#"[0]
"#,
        0,
    );
    // z_quoted_star_empty_ifs
    check(
        r#"set -- a b; IFS=; echo "$*"; x="$*"; echo $x"#,
        r#"ab
ab
"#,
        0,
    );
    // z_unquoted_at_empty_ifs
    check(
        r#"set -- "a b" c; IFS=; printf '[%s]' $@; echo; printf '[%s]' $*; echo"#,
        r#"[a b][c]
[a b][c]
"#,
        0,
    );
    // z_ifs_unset_star_join
    check(
        r#"set -- a b; unset IFS; echo "$*""#,
        r#"a b
"#,
        0,
    );
    // z_default_assign_array_elem
    check(
        r#"a=(); : ${a[2]:=z}; echo ${!a[@]} ${a[2]}"#,
        r#"2 z
"#,
        0,
    );
    // z_declare_p_function_local_array
    check(
        r#"f() { local -a la=(1 "2 3"); declare -p la; }; f"#,
        r#"declare -a la=([0]="1" [1]="2 3")
"#,
        0,
    );
    // z_case_conversion_pattern_each
    check(
        r#"a=(apple banana); echo "${a[@]^}" "${a[@]^^[an]}""#,
        r#"Apple Banana Apple bANANA
"#,
        0,
    );
    // z_replace_in_array_quoted_spaces
    check(
        r#"a=("a b" "c b"); printf '[%s]' "${a[@]// /_}"; echo"#,
        r#"[a_b][c_b]
"#,
        0,
    );
    // z_substring_on_array_elem
    check(
        r#"a=(hello world); echo ${a[1]:1:3}"#,
        r#"orl
"#,
        0,
    );
    // z_length_of_positional
    check(
        r#"set -- abc de; echo ${#1} ${#2}"#,
        r#"3 2
"#,
        0,
    );
}

#[test]
fn declare_f_prints_functions_through_the_ast_printer() {
    // Output must also re-evaluate to the same function (`eval` at the end).
    check(
        "f() { echo f; }; g() { echo g; }; declare -f; echo ===; declare -F; echo ===; declare -f g f; echo \"st=$?\"; declare -f nosuch; echo \"st=$?\"; declare -F f nosuch; echo \"st=$?\"; typeset -f g; export -f f; echo \"ex=$?\"; eval \"$(declare -f g | sed s/g/h/)\"; h",
        "f () \n{ \n    echo f\n}\ng () \n{ \n    echo g\n}\n===\ndeclare -f f\ndeclare -f g\n===\ng () \n{ \n    echo g\n}\nf () \n{ \n    echo f\n}\nst=0\nst=1\nf\nst=1\ng () \n{ \n    echo g\n}\nex=0\nh\n",
        0,
    );
}
