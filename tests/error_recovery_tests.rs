//! How a script carries on after an error stops a command, as in bash.
//!
//! A failed expansion (`$((1/0))`, `${}`, failglob) or a refused assignment
//! (`R=2` with `R` readonly) skips the rest of its input line, and the script
//! continues with the next line with `$?` set to 1. `eval`, `source` and trap
//! actions recover line by line inside their own text; a subshell exits.
//! `set -u`, `${x:?}` and arithmetic errors in array indexes and integer
//! variables end the script, and `set -e` also ends it, except after an
//! arithmetic expansion error. jsh used to end the whole script every time.
use std::process::{Command, Stdio};

fn run(script: &str) -> (String, i32) {
    let dir = tempfile::tempdir().expect("temp dir");
    let output = Command::new(env!("CARGO_BIN_EXE_jsh"))
        .args(["--norc", "-c", script])
        .current_dir(dir.path())
        .stdin(Stdio::null())
        .output()
        .expect("run jsh");
    (
        String::from_utf8_lossy(&output.stdout).into_owned(),
        output.status.code().unwrap_or(-1),
    )
}

fn assert_run(script: &str, stdout: &str, status: i32) {
    assert_eq!(
        run(script),
        (stdout.to_string(), status),
        "script: {script:?}"
    );
}

#[test]
fn an_expansion_error_skips_the_rest_of_its_line() {
    assert_run("echo a; echo $((1/0)); echo b\necho c", "a\nc\n", 0);
    assert_run("echo ${}; echo same\necho next st=$?", "next st=1\n", 0);
    assert_run(
        "shopt -s failglob; echo /nonexist*; echo same\necho next st=$?",
        "next st=1\n",
        0,
    );
    assert_run(
        "case $((1/0)) in *) ;; esac; echo same\necho next",
        "next\n",
        0,
    );
}

#[test]
fn the_line_is_the_whole_top_level_command() {
    // A compound command spanning lines is one line of input, up to the
    // newline after its last command.
    assert_run(
        "if true; then\n echo $((1/0))\n echo after\nfi; echo same\necho next",
        "next\n",
        0,
    );
    assert_run(
        "echo a &&\necho $((1/0)) &&\necho b; echo same\necho next",
        "a\nnext\n",
        0,
    );
    // The error unwinds functions and loops.
    assert_run(
        "f() { echo $((1/0)); echo in-f; }; f; echo same\necho next",
        "next\n",
        0,
    );
    assert_run(
        "for i in 1 2; do echo $i $((1/0)); done; echo same\necho next",
        "next\n",
        0,
    );
}

#[test]
fn a_refused_assignment_skips_the_rest_of_its_line() {
    assert_run(
        "echo ok; readonly R=1\nR=2\necho next st=$?\nR=3; echo no\necho last st=$?",
        "ok\nnext st=1\nlast st=1\n",
        0,
    );
    assert_run(
        "f() { local -r L=1; L=2; echo in-f; }; f; echo same\necho next st=$?",
        "next st=1\n",
        0,
    );
    // A prefix assignment only warns; the command still runs.
    assert_run(
        "readonly R=1; R=2 echo hi; echo same st=$?",
        "hi\nsame st=0\n",
        0,
    );
}

#[test]
fn eval_source_and_traps_recover_inside_their_own_text() {
    assert_run(
        "eval 'echo e1 $((1/0)); echo e2\necho e3'; echo after st=$?\necho next",
        "e3\nafter st=0\nnext\n",
        0,
    );
    assert_run(
        "f() { eval 'echo $((1/0))'; echo in-f; }; f; echo same",
        "in-f\nsame\n",
        0,
    );
    assert_run(
        "printf 'echo s1 $((1/0)); echo s2\\necho s3\\n' > f.sh; source ./f.sh; echo after st=$?",
        "s3\nafter st=0\n",
        0,
    );
    assert_run(
        "trap 'echo t1 $((1/0)); echo t2\necho t3' EXIT; echo body",
        "body\nt3\n",
        0,
    );
}

#[test]
fn a_subshell_exits_instead() {
    assert_run(
        "( echo $((1/0)); echo sub-b ); echo st=$?\necho next",
        "st=1\nnext\n",
        0,
    );
    assert_run(
        "x=$(echo c1 $((1/0)); echo c2\necho c3); echo \"st=$? x=$x\"",
        "st=1 x=\n",
        0,
    );
    assert_run(
        "( eval 'echo $((1/0))\necho e2'; echo after )\necho next",
        "next\n",
        0,
    );
}

#[test]
fn errexit_ends_the_script_except_after_arithmetic_errors() {
    assert_run(
        "readonly R=1; set -e; R=2 || echo handled; echo same",
        "",
        1,
    );
    assert_run(
        "set -e; shopt -s failglob; if echo /nonexist*; then :; fi; echo same",
        "",
        1,
    );
    assert_run(
        "set -e; echo $((1/0)); echo same\necho next st=$?",
        "next st=1\n",
        0,
    );
    // No ERR trap either way.
    assert_run(
        "trap 'echo ERR' ERR; echo $((1/0)); echo same\necho next st=$?",
        "next st=1\n",
        0,
    );
}

#[test]
fn some_errors_end_the_script() {
    assert_run("declare -i n; n=1/0; echo same\necho next", "", 1);
    assert_run("a=(1 2); echo ${a[1/0]}; echo same\necho next", "", 1);
    assert_run("a[1/0]=x; echo same\necho next", "", 1);
    assert_run(
        "trap 'echo exit-trap st=$?' EXIT; a=(1); echo ${a[1/0]}",
        "exit-trap st=1\n",
        1,
    );
}

#[test]
fn unset_parameters_end_the_script_with_status_127() {
    assert_run("set -u; echo $nope; echo same\necho next", "", 127);
    assert_run("set -u; f() { echo $nope; }; f", "", 127);
    assert_run("set -u; eval 'echo $nope'; echo after", "", 127);
    assert_run("set -u; let nope+1; echo same", "", 127);
    assert_run("echo ${x:?gone}; echo same\necho next", "", 127);
    assert_run(
        "trap 'echo exit-trap st=$?' EXIT; set -u; echo $nope",
        "exit-trap st=127\n",
        127,
    );
    // A subshell reports 1.
    assert_run("( set -u; echo $nope ); echo st=$?", "st=1\n", 0);
}

#[test]
fn a_non_interactive_shell_does_not_announce_background_jobs() {
    let output = Command::new(env!("CARGO_BIN_EXE_jsh"))
        .args(["--norc", "-c", "sleep 0.1 & wait; echo done"])
        .stdin(Stdio::null())
        .output()
        .expect("run jsh");
    assert_eq!(String::from_utf8_lossy(&output.stdout), "done\n");
    assert_eq!(String::from_utf8_lossy(&output.stderr), "");
}

#[test]
fn only_a_command_string_exits_127_on_an_unset_parameter() {
    // Bash's `-c` string exits with 127, but a script file or standard input
    // exits with 1.
    let dir = tempfile::tempdir().expect("temp dir");
    let script = dir.path().join("u.sh");
    std::fs::write(&script, "trap 'echo trap st=$?' EXIT\nset -u\necho $nope\n").unwrap();
    let output = Command::new(env!("CARGO_BIN_EXE_jsh"))
        .arg("--norc")
        .arg(&script)
        .stdin(Stdio::null())
        .output()
        .expect("run jsh");
    assert_eq!(String::from_utf8_lossy(&output.stdout), "trap st=1\n");
    assert_eq!(output.status.code(), Some(1));

    let output = Command::new(env!("CARGO_BIN_EXE_jsh"))
        .arg("--norc")
        .stdin(std::fs::File::open(&script).unwrap())
        .output()
        .expect("run jsh");
    assert_eq!(String::from_utf8_lossy(&output.stdout), "trap st=1\n");
    assert_eq!(output.status.code(), Some(1));

    assert_run(
        &format!("source {}", script.display()),
        "trap st=127\n",
        127,
    );
}
