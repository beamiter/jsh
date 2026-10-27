//! Bash 5.2 parity for the execution engine: `break`/`continue N`, traps
//! (signals, EXIT, ERR, DEBUG, RETURN), the call stack (`$LINENO`,
//! `FUNCNAME`, `BASH_SOURCE`, `BASH_LINENO`, `caller`, `$_`,
//! `BASH_COMMAND`), the `time` reserved word, `{name}` and moving
//! redirections, `select`, `coproc`, `local -` and
//! `command_not_found_handle`.
//!
//! Every expected output below was produced by running the same script with
//! `bash --norc --noprofile -c` (bash 5.2) in an empty directory; bash is not
//! needed to run these tests.

use std::fs;
use std::os::unix::process::ExitStatusExt;
use std::path::Path;
use std::process::{Command, Output, Stdio};
use std::time::{Duration, Instant};

/// Generous: a broken trap or wait must fail the test, not hang the suite.
const DEADLINE: Duration = Duration::from_secs(30);

fn run_in(dir: &Path, args: &[&str]) -> Output {
    let mut child = Command::new(env!("CARGO_BIN_EXE_jsh"))
        .args(args)
        .current_dir(dir)
        .env("HOME", dir)
        .env("LC_ALL", "C.UTF-8")
        .env_remove("TIMEFORMAT")
        .env_remove("COLUMNS")
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .expect("spawn jsh");
    let started = Instant::now();
    loop {
        if child.try_wait().expect("poll jsh").is_some() {
            return child.wait_with_output().expect("collect jsh output");
        }
        if started.elapsed() > DEADLINE {
            let _ = child.kill();
            let output = child.wait_with_output().expect("collect jsh output");
            panic!(
                "jsh did not finish: {args:?}\nstdout: {}\nstderr: {}",
                String::from_utf8_lossy(&output.stdout),
                String::from_utf8_lossy(&output.stderr)
            );
        }
        std::thread::sleep(Duration::from_millis(5));
    }
}

/// Exit status, or minus the signal number for a process killed by one
/// (Python's convention, in which the expectations were recorded).
fn status_of(output: &Output) -> Option<i32> {
    output
        .status
        .code()
        .or_else(|| output.status.signal().map(|signal| -signal))
}

/// Run `script` with `jsh -c` in a fresh directory and compare stdout and
/// the exit status with bash's.
fn expect(script: &str, stdout: &str, status: i32) {
    let dir = tempfile::tempdir().expect("tempdir");
    let output = run_in(dir.path(), &["--norc", "-c", script]);
    assert_eq!(
        String::from_utf8_lossy(&output.stdout),
        stdout,
        "script: {script}\nstderr: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert_eq!(
        status_of(&output),
        Some(status),
        "script: {script}\nstderr: {}",
        String::from_utf8_lossy(&output.stderr)
    );
}

/// A trapped signal interrupts `wait`, which returns 128+N, and the trap
/// runs. The sender repeats until it is killed, so the signal is certain to
/// land while `wait` blocks however the processes are scheduled; signals
/// that arrive earlier just run the (cheap) trap between commands.
#[test]
fn trapped_signal_interrupts_wait() {
    expect(
        r#"trap 'n=$((n+1))' USR1; pid=$$
(while kill -USR1 $pid 2>/dev/null; do sleep 0.02; done) & sender=$!
sleep 30 & sleeper=$!
wait $sleeper; echo "wait=$?"
kill $sender $sleeper
[ "$n" -ge 1 ] && echo trapped"#,
        "wait=138\ntrapped\n",
        0,
    );
}

/// A script is bash's `main` frame; sourced files and functions stack on it.
#[test]
fn call_stack_in_a_script_matches_bash() {
    let dir = tempfile::tempdir().expect("tempdir");
    fs::write(
        dir.path().join("main.sh"),
        r#"echo "top [${FUNCNAME[*]}] ${#FUNCNAME[@]} [${BASH_SOURCE[*]}] [${BASH_LINENO[*]}] $LINENO"
h() { echo "h [${BASH_LINENO[*]}] [${FUNCNAME[*]}] [${BASH_SOURCE[*]}]"; caller; caller 0; caller 1; echo st=$?; }
h
k() { h; }
k
. ./lib.sh
echo "after source $LINENO"
caller; echo st=$?
"#,
    )
    .expect("write main.sh");
    fs::write(
        dir.path().join("lib.sh"),
        r#"echo "lib [${FUNCNAME[*]}] [${BASH_SOURCE[*]}] [${BASH_LINENO[*]}] $LINENO"
caller; caller 0
h
"#,
    )
    .expect("write lib.sh");
    let output = run_in(dir.path(), &["main.sh"]);
    assert_eq!(output.status.code(), Some(0));
    assert_eq!(
        String::from_utf8_lossy(&output.stdout),
        "top [] 0 [main.sh] [0] 1
h [3 0] [h main] [main.sh main.sh]
3 main.sh
3 main main.sh
st=1
h [4 5 0] [h k main] [main.sh main.sh main.sh]
4 main.sh
4 k main.sh
5 main main.sh
st=0
lib [] [./lib.sh main.sh] [6 0] 1
6 main.sh
6 main main.sh
h [3 6 0] [h source main] [main.sh ./lib.sh main.sh]
3 ./lib.sh
3 source ./lib.sh
6 main main.sh
st=0
after source 7
0 NULL
st=0
"
    );
}

/// A non-numeric count is fatal for a non-interactive shell (status 128),
/// as for any POSIX special builtin error in bash.
#[test]
fn non_numeric_loop_count_is_fatal() {
    let dir = tempfile::tempdir().expect("tempdir");
    let output = run_in(
        dir.path(),
        &[
            "--norc",
            "-c",
            "for i in 1; do break x; echo inloop; done; echo st=$?",
        ],
    );
    assert_eq!(output.status.code(), Some(128));
    assert_eq!(String::from_utf8_lossy(&output.stdout), "");
    assert!(String::from_utf8_lossy(&output.stderr).contains("break: x: numeric argument required"));
}

/// `break 2` leaves both loops.
#[test]
fn break_n_leaves_enclosing_loops() {
    expect(
        r##"for i in 1 2; do for j in 1 2; do echo $i$j; break 2; done; done; echo after"##,
        r##"11
after
"##,
        0,
    );
}

/// `continue 2` skips the rest of the outer iteration too.
#[test]
fn continue_n_continues_outer_loop() {
    expect(
        r##"for i in 1 2; do for j in 1 2; do continue 2; echo no; done; echo no2; done; echo done"##,
        r##"done
"##,
        0,
    );
}

/// A count larger than the nesting depth acts on the outermost loop.
#[test]
fn loop_count_beyond_depth_means_outermost() {
    expect(
        r##"for i in 1 2; do for j in a b; do echo $i$j; break 5; done; done; for i in 1 2; do for j in a b; do echo $i$j; continue 5; done; done; echo after"##,
        r##"1a
1a
2a
after
"##,
        0,
    );
}

/// `break 0` is an error (status 1) that leaves every enclosing loop.
#[test]
fn break_zero_reports_and_leaves_every_loop() {
    expect(
        r##"for i in 1 2; do for j in a b; do break 0; echo after-break; done; echo outer$i; done; echo st=$?"##,
        r##"st=1
"##,
        0,
    );
}

/// Counted control flow crosses `while`/`until` loops, including from their conditions.
#[test]
fn continue_n_through_while_and_until() {
    expect(
        r##"i=0; while [ $i -lt 3 ]; do i=$((i+1)); for j in 1 2; do continue 2; done; echo no; done; echo $i
for o in 1 2; do x=0; while x=$((x+1)); [ $x -lt 3 ] || break 2; do echo $o$x; done; echo after-while; done; echo end
x=0; until x=$((x+1)); if [ $x -lt 3 ]; then continue; fi; true; do echo unreachable; done; echo x=$x"##,
        r##"3
11
12
end
x=1
"##,
        0,
    );
}

/// Whitespace and a sign are accepted, as by bash's legal_number.
#[test]
fn loop_count_accepts_bash_number_syntax() {
    expect(
        r##"for i in 1 2; do break " 2"; done; for i in 1 2; do break +1; echo x; done; echo st=$?"##,
        r##"st=0
"##,
        0,
    );
}

/// A function's `break` cannot leave its caller's loop.
#[test]
fn functions_do_not_see_the_callers_loops() {
    expect(
        r##"f() { break; }; for i in 1 2; do f; echo $i; done 2>/dev/null"##,
        r##"1
2
"##,
        0,
    );
}

/// `select` is a loop for `break N`.
#[test]
fn select_counts_as_a_loop() {
    expect(
        r##"printf '1\n2\n' | { select x in a b; do for i in 1; do break 2; done; echo no; done; echo out; } 2>/dev/null"##,
        r##"out
"##,
        0,
    );
}

/// `trap` and `trap -p` list in signal order with re-readable quoting.
#[test]
fn trap_listing_is_ordered_and_quoted() {
    expect(
        r##"trap "echo a" INT TERM 10 SIGHUP; trap "" QUIT; trap "echo 'q'" EXIT; trap; echo ---; trap -p INT QUIT; echo ---; trap -p USR2; echo st=$?"##,
        r##"trap -- 'echo '\''q'\''' EXIT
trap -- 'echo a' SIGHUP
trap -- 'echo a' SIGINT
trap -- '' SIGQUIT
trap -- 'echo a' SIGUSR1
trap -- 'echo a' SIGTERM
---
trap -- 'echo a' SIGINT
trap -- '' SIGQUIT
---
st=0
q
"##,
        0,
    );
}

/// EXIT comes first and DEBUG, ERR, RETURN last.
#[test]
fn trap_listing_includes_shell_conditions() {
    expect(
        r##"trap "echo e" EXIT; trap "echo r" RETURN; trap "echo er" ERR; trap -p; trap - EXIT"##,
        r##"trap -- 'echo e' EXIT
trap -- 'echo er' ERR
trap -- 'echo r' RETURN
"##,
        0,
    );
}

/// Numbers, lowercase names and the SIG prefix name the same signal; one operand or a leading number resets.
#[test]
fn trap_condition_spellings() {
    expect(
        r##"trap -- "echo dd" INT; trap -p INT; trap INT; trap -p INT; trap 'echo a' usr1; trap -p; trap 10; trap 'echo b' sigint 0; trap -p; trap 0 2; trap -p; echo end"##,
        r##"trap -- 'echo dd' SIGINT
trap -- 'echo a' SIGUSR1
trap -- 'echo b' EXIT
trap -- 'echo b' SIGINT
end
"##,
        0,
    );
}

/// An invalid condition fails with status 1 but valid ones still apply.
#[test]
fn trap_rejects_unknown_conditions() {
    expect(
        r##"trap "echo x" FOO EXIT 2>/dev/null; echo st=$?; trap "echo x" 99 2>/dev/null; echo st=$?; trap -p EXIT FOO 2>/dev/null; echo st=$?; trap - EXIT"##,
        r##"st=1
st=1
trap -- 'echo x' EXIT
st=1
"##,
        0,
    );
}

/// `trap -l` prints bash's five-column table.
#[test]
fn trap_dash_l_matches_bash_layout() {
    expect(
        r##"trap -l | head -3; trap -l | tail -1"##,
        r##" 1) SIGHUP	 2) SIGINT	 3) SIGQUIT	 4) SIGILL	 5) SIGTRAP
 6) SIGABRT	 7) SIGBUS	 8) SIGFPE	 9) SIGKILL	10) SIGUSR1
11) SIGSEGV	12) SIGUSR2	13) SIGPIPE	14) SIGALRM	15) SIGTERM
63) SIGRTMAX-1	64) SIGRTMAX	
"##,
        0,
    );
}

/// A subshell resets caught traps, but `trap -p` still shows the parent's until it sets its own.
#[test]
fn subshells_show_but_do_not_keep_parent_traps() {
    expect(
        r##"trap "echo hi" INT; echo "$(trap -p INT)"; (trap -p INT); (trap "echo z" USR1; trap -p)"##,
        r##"trap -- 'echo hi' SIGINT
trap -- 'echo hi' SIGINT
trap -- 'echo z' SIGUSR1
"##,
        0,
    );
}

/// Trapped INT, TERM, HUP and USR1 run their action and the shell carries on.
#[test]
fn trapped_signal_runs_action_and_continues() {
    expect(
        r##"trap 'echo int' INT; trap 'echo term' TERM; trap 'echo hup' HUP; trap 'echo usr1' 10; kill -INT $$; kill -TERM $$; kill -HUP $$; kill -USR1 $$; echo after"##,
        r##"int
term
hup
usr1
after
"##,
        0,
    );
}

/// A signal that arrives during a foreground command runs its trap after the command.
#[test]
fn trap_waits_for_the_foreground_command() {
    expect(
        r##"trap 'echo got' USR1; sh -c 'kill -USR1 $PPID; echo child-done'; echo after"##,
        r##"child-done
got
after
"##,
        0,
    );
}

/// Another signal's trap runs inside a trap action, as in bash.
#[test]
fn signal_trap_nests_inside_another_signals_trap() {
    expect(
        r##"pid=$$; trap 'echo usr2' USR2; trap 'echo usr1-start; kill -USR2 $pid; echo usr1-end' USR1; kill -USR1 $$; echo done"##,
        r##"usr1-start
usr2
usr1-end
done
"##,
        0,
    );
}

/// Real-time signals are trapped by name and listed with bash's names.
#[test]
fn realtime_signal_trap() {
    expect(
        r##"trap 'echo rt' RTMIN+1; trap -p; kill -35 $$; echo after"##,
        r##"trap -- 'echo rt' SIGRTMIN+1
rt
after
"##,
        0,
    );
}

/// ERR and `set -e` apply to the commands of trap actions, EXIT included.
#[test]
fn trap_actions_run_as_command_lists() {
    expect(
        r##"trap 'echo err' ERR; trap 'false; echo t' USR1; kill -USR1 $$; trap 'false' EXIT; echo x"##,
        r##"err
t
x
err
"##,
        0,
    );
}

/// A failure under `set -e` in a trap action ends the shell.
#[test]
fn errexit_inside_a_trap_action_exits() {
    expect(
        r##"set -e; trap 'false; echo in-trap' USR1; kill -USR1 $$; echo after"##,
        r##""##,
        1,
    );
}

/// `return` in a trap action returns from the interrupted function.
#[test]
fn trap_action_can_return_from_function() {
    expect(
        r##"f() { trap 'return 3' USR1; kill -USR1 $$; echo no; }; f; echo "st=$?"; trap - USR1; for i in 1 2 3; do trap 'break' USR1; kill -USR1 $$; echo "i=$i"; done; echo end"##,
        r##"st=3
end
"##,
        0,
    );
}

/// `$?` after a trap action is the interrupted command's status.
#[test]
fn trap_action_preserves_status() {
    expect(
        r##"trap 'false' USR1; true; kill -USR1 $$; echo "st=$?""##,
        r##"st=0
"##,
        0,
    );
}

/// The `trap cleanup EXIT INT TERM` idiom.
#[test]
fn cleanup_trap_on_exit_int_and_term() {
    expect(
        r##"cleanup() { echo "cleaning $1"; }; trap 'cleanup term' TERM; trap 'cleanup exit' EXIT; kill -TERM $$; echo working"##,
        r##"cleaning term
working
cleaning exit
"##,
        0,
    );
}

/// `exit` inside a trap action ends the shell with that status, after the EXIT trap.
#[test]
fn trap_action_can_exit() {
    expect(
        r##"trap 'echo bye; exit 7' USR1; trap 'echo exiting $?' EXIT; kill -USR1 $$; echo not-reached"##,
        r##"bye
exiting 7
"##,
        7,
    );
}

/// `trap - SIG` puts back the default, so the signal kills the shell again.
#[test]
fn reset_trap_restores_default_action() {
    expect(
        r##"trap 'echo x' USR1; trap - USR1; kill -USR1 $$; echo survived"##,
        r##""##,
        -10,
    );
}

/// `trap '' SIG` ignores the signal in children too.
#[test]
fn ignored_signal_is_inherited_by_children() {
    expect(
        r##"trap '' USR1 TERM; sh -c 'kill -USR1 $$; kill -TERM $$; echo child-survived'; echo parent-alive"##,
        r##"child-survived
parent-alive
"##,
        0,
    );
}

/// A subshell does not run its parent's signal trap; the default action applies.
#[test]
fn subshell_resets_caught_signal() {
    expect(
        r##"trap 'echo parent' USR1; ( sh -c 'kill -USR1 $PPID'; echo sub-alive ); echo "st=$?""##,
        r##"st=138
"##,
        0,
    );
}

/// `$?` inside the EXIT trap is the exit status.
#[test]
fn exit_trap_sees_exit_status() {
    expect(
        r##"trap 'echo "exit=$?"' EXIT; exit 3"##,
        r##"exit=3
"##,
        3,
    );
}

/// A subshell's EXIT trap runs when the subshell ends.
#[test]
fn subshell_runs_its_own_exit_trap() {
    expect(
        r##"(trap 'echo sub-exit $?' EXIT; echo in-sub; exit 4); echo "after $?"; x=$(trap 'echo cs-exit' EXIT; echo hi); echo "[$x]""##,
        r##"in-sub
sub-exit 4
after 4
[hi
cs-exit]
"##,
        0,
    );
}

/// ERR fires for failing commands inside groups and bodies, once.
#[test]
fn err_trap_fires_once_per_failure() {
    expect(
        r##"trap 'echo err' ERR; { false; }; echo a; { false; echo x; }; if true; then false; fi; for i in 1; do false; done; case x in x) false;; esac; echo b"##,
        r##"err
a
err
x
err
err
err
b
"##,
        0,
    );
}

/// Conditions, negations and non-final `&&`/`||` operands do not fire ERR.
#[test]
fn err_trap_skips_conditional_contexts() {
    expect(
        r##"trap 'echo err' ERR; while false; do :; done; ! true; false && true; true && false; false | true; true | false; if false; then :; fi; echo c"##,
        r##"err
err
c
"##,
        0,
    );
}

/// A failing subshell, `((...))` and `[[...]]` fire ERR.
#[test]
fn err_trap_for_subshell_arith_and_cond() {
    expect(
        r##"trap 'echo err' ERR; (false); (( 0 )); [[ -z x ]]; echo d"##,
        r##"err
err
err
d
"##,
        0,
    );
}

/// Without `set -E`, a function does not see the ERR trap, but its failing call fires it.
#[test]
fn err_trap_not_inherited_by_functions() {
    expect(
        r##"trap 'echo err' ERR; f() { false; echo in; trap -p ERR; }; f; g() { false; }; g; g && echo yes; echo out; trap -p ERR"##,
        r##"in
err
out
trap -- 'echo err' ERR
"##,
        0,
    );
}

/// An ERR trap a function sets for itself stays after it returns.
#[test]
fn err_trap_set_in_function_persists() {
    expect(
        r##"trap 'echo err' ERR; f() { trap 'echo ferr' ERR; false; }; f; false"##,
        r##"ferr
ferr
ferr
"##,
        1,
    );
}

/// With `set -E` the trap fires inside functions, subshells and substitutions too.
#[test]
fn errtrace_inherits_err_trap() {
    expect(
        r##"set -E; trap 'echo err' ERR; f() { false; }; f; (false); x=$(false; echo z); echo "out $x""##,
        r##"err
err
err
err
out err
z
"##,
        0,
    );
}

/// `eval false` fails inside eval and as a command, as in bash.
#[test]
fn eval_failure_fires_err_twice() {
    expect(
        r##"trap 'echo err' ERR; eval false; echo m"##,
        r##"err
err
m
"##,
        0,
    );
}

/// DEBUG runs before simple commands and loop/case headers, with BASH_COMMAND.
#[test]
fn debug_trap_reports_each_command() {
    expect(
        r##"trap 'echo "d:$BASH_COMMAND"' DEBUG; for i in 1 2; do :; done; case x in x) :;; esac; f() { echo in f; }; f; x=$(echo cs); echo "$x" >/dev/null; { echo grp; }; while false; do :; done; true | cat; a=1 b=2; echo   "spaced   arg"  2>&1"##,
        r##"d:for i in 1 2
d::
d:for i in 1 2
d::
d:case x in 
d::
d:f
in f
d:x=$(echo cs)
d:echo "$x" > /dev/null
d:echo grp
grp
d:false
d:true
d:cat
d:a=1 b=2
d:echo "spaced   arg" 2>&1
spaced   arg
"##,
        0,
    );
}

/// DEBUG sees the previous status and the upcoming command's line.
#[test]
fn debug_trap_status_and_lineno() {
    expect(
        r##"trap 'echo "dbg $? $LINENO"' DEBUG; false
echo x"##,
        r##"dbg 0 1
dbg 1 2
x
"##,
        0,
    );
}

/// DEBUG also runs before the commands of other trap actions.
#[test]
fn debug_trap_runs_inside_other_traps() {
    expect(
        r##"trap 'echo dbg' DEBUG; trap 'echo err' ERR; trap 'echo usr1' USR1; false; kill -USR1 $$; trap - DEBUG"##,
        r##"dbg
dbg
dbg
dbg
err
dbg
dbg
usr1
dbg
"##,
        0,
    );
}

/// `trap - DEBUG` stops it.
#[test]
fn debug_trap_off() {
    expect(
        r##"trap 'echo d' DEBUG; trap - DEBUG; echo plain"##,
        r##"d
plain
"##,
        0,
    );
}

/// With `set -T` DEBUG runs inside functions, once more as they start.
#[test]
fn functrace_passes_debug_to_functions() {
    expect(
        r##"set -T; trap 'echo "d:$BASH_COMMAND"' DEBUG; f() { echo infunc; }; f"##,
        r##"d:f
d:f
d:echo infunc
infunc
"##,
        0,
    );
}

/// A DEBUG trap set inside a function stays after it returns.
#[test]
fn debug_trap_set_in_function_persists() {
    expect(
        r##"f() { trap 'echo "fd:$BASH_COMMAND"' DEBUG; echo a; }; f; echo b; trap - DEBUG"##,
        r##"fd:echo a
a
fd:echo b
b
fd:trap - DEBUG
"##,
        0,
    );
}

/// RETURN fires when a function that set it, or a sourced file, finishes.
#[test]
fn return_trap_fires_for_functions_and_source() {
    expect(
        r##"f() { trap 'echo "ret $?"' RETURN; false; }; f; echo "st=$?"; echo 'echo in-src' > s.sh; . ./s.sh; echo after"##,
        r##"ret 1
st=1
in-src
ret 0
after
"##,
        0,
    );
}

/// A global RETURN trap is hidden from functions unless `set -T`.
#[test]
fn return_trap_not_inherited_without_functrace() {
    expect(
        r##"trap 'echo ret' RETURN; f() { :; }; f; echo x; set -T; f; echo y"##,
        r##"x
ret
y
"##,
        0,
    );
}

/// After `return N` the RETURN trap sees the previous command's status; the call returns N.
#[test]
fn return_trap_status_is_last_command() {
    expect(
        r##"f() { trap 'echo "ret $?"' RETURN; return 3; }; f; echo "st=$?""##,
        r##"ret 0
st=3
"##,
        0,
    );
}

/// LINENO for continuations, multi-line words, functions, eval and here-documents.
#[test]
fn lineno_follows_bash() {
    expect(
        r##"echo a \
  $LINENO
echo $LINENO; true &&
echo $LINENO
f() {
  echo "f $LINENO"
}
f
for i in $LINENO; do echo "for $i $LINENO"; done
eval "echo eval \$LINENO
echo eval2 \$LINENO"
echo "multi
line $LINENO"
(
echo sub $LINENO
)
cat <<END
hd $LINENO
END
x=1 \
  y=2
echo end $LINENO"##,
        r##"a 1
3
4
f 6
for 9 9
eval 11
eval2 12
multi
line 13
sub 15
hd 17
end 22
"##,
        0,
    );
}

/// A command substitution continues the line count of its command, from its first non-blank line.
#[test]
fn command_substitution_lines() {
    expect(
        r##"x=$(echo $LINENO); echo $x
y=$(

echo $LINENO
); echo $y"##,
        r##"1
5
"##,
        0,
    );
}

/// LINENO inside the ERR trap is the failing command's line.
#[test]
fn err_trap_reports_failing_line() {
    expect(
        r##"trap 'echo "err at $LINENO"' ERR
true
false
f() {
  false
}
f"##,
        r##"err at 3
err at 7
"##,
        1,
    );
}

/// FUNCNAME, BASH_LINENO, BASH_SOURCE and caller for `-c` code.
#[test]
fn call_stack_arrays_in_c_mode() {
    expect(
        r##"h() { echo "h ${BASH_LINENO[*]} ${FUNCNAME[*]} ${BASH_SOURCE[*]}"; caller; caller 0 || echo none; }
h
k() { h; }
k
echo "[${FUNCNAME[*]}] ${#FUNCNAME[@]} ${#BASH_LINENO[@]}"; caller; echo st=$?"##,
        r##"h 2 h environment
2 NULL
none
h 3 4 h k environment environment
3 environment
3 k environment
[] 0 0
st=1
"##,
        0,
    );
}

/// A sourced file is a frame; its functions keep their file.
#[test]
fn call_stack_through_sourced_files() {
    expect(
        r##"printf 'g() { echo "${FUNCNAME[*]} | ${BASH_SOURCE[*]} | ${BASH_LINENO[*]}"; }\ncaller\n' > lib.sh
. ./lib.sh
f() { g; }
f"##,
        r##"2 NULL
g f | ./lib.sh environment | 3 4
"##,
        0,
    );
}

/// `$_` is the previous simple command's last argument.
#[test]
fn underscore_is_last_argument() {
    expect(
        r##"true; echo "[$_]"; f() { echo in "$_"; }; true x; f arg1 arg2; echo "[$_]"; echo "a b" >/dev/null; echo "[$_]"; for i in 1 2; do :; done; echo "[$_]"; x=$(echo q r); echo "[$_]"; true v; eval 'echo ev: $_'; echo "e2: $_""##,
        r##"[true]
in x
[arg2]
[a b]
[:]
[]
ev: v
e2: echo ev: $_
"##,
        0,
    );
}

/// BASH_COMMAND, also inside an ERR trap.
#[test]
fn bash_command_names_current_command() {
    expect(
        r##"echo "$BASH_COMMAND"; trap 'echo "failed: $BASH_COMMAND"' ERR; false arg"##,
        r##"echo "$BASH_COMMAND"
failed: false arg
"##,
        1,
    );
}

/// `time` keeps the pipeline status and writes to the shell's stderr.
#[test]
fn time_reports_on_stderr_and_keeps_status() {
    expect(
        r##"TIMEFORMAT=T; time false; echo st=$?; time true 2>/dev/null | false; echo st=$?; { time ! false; echo st=$?; ! time false; echo st=$?; } 2>&1; time=5; echo $time time"##,
        r##"st=1
st=1
T
st=0
T
st=0
5 time
"##,
        0,
    );
}

/// TIMEFORMAT escapes, `-p`, the default layout and an empty format.
#[test]
fn time_formats() {
    expect(
        r##"TIMEFORMAT='a%%b %R|%3lR|%0R|%U|%1S|%P'; { time true; } 2>&1 | sed 's/[0-9][0-9]*/N/g'; unset TIMEFORMAT; { time true; } 2>&1 | sed 's/[0-9]/N/g'; { time -p true; } 2>&1 | sed 's/[0-9]/N/g'; TIMEFORMAT=; { time true; } 2>&1; TIMEFORMAT=x%q; { time true; } 2>/dev/null; echo done"##,
        r##"a%b N.N|NmN.Ns|N|N.N|N.N|N.N

real	NmN.NNNs
user	NmN.NNNs
sys	NmN.NNNs
real N.NN
user N.NN
sys N.NN
done
"##,
        0,
    );
}

/// `time` alone and with compound commands.
#[test]
fn time_without_pipeline_and_compounds() {
    expect(
        r##"TIMEFORMAT=t; { time { sleep 0; }; time (true); time for i in 1; do :; done; time; echo st=$?; } 2>&1"##,
        r##"t
t
t
t
st=0
"##,
        0,
    );
}

/// `{name}>file` allocates a descriptor from 10 and keeps it open.
#[test]
fn fd_variable_open_write_read_close() {
    expect(
        r##"exec {fd}>out.txt; [ $fd -ge 10 ] && echo ge10; echo hi >&$fd; exec {fd}>&-; cat out.txt; echo data > in.txt; exec {rfd}<in.txt; read -r l <&$rfd; echo $l; exec {rfd}<&-"##,
        r##"ge10
hi
data
"##,
        0,
    );
}

/// The variable is set by builtins and functions, not by external commands.
#[test]
fn fd_variable_on_builtins_functions_and_externals() {
    expect(
        r##"/bin/true {fd}>x.txt; echo "ext[$fd]"; true {g}>y.txt; echo "bi[$g]"; f() { echo inf >&$h; }; f {h}>z.txt; echo "fn[$h]"; cat z.txt"##,
        r##"ext[]
bi[10]
fn[11]
inf
"##,
        0,
    );
}

/// `{n}>&3`, array elements, and closing through an unset name.
#[test]
fn fd_variable_dup_array_and_errors() {
    expect(
        r##"exec 3>o.txt; exec {n}>&3; echo viaN >&$n; cat o.txt; arr=(); exec {arr[2]}>z.txt; echo "[${arr[2]}]"; exec {nope}<&- 2>/dev/null; echo st=$?"##,
        r##"viaN
[11]
st=1
"##,
        0,
    );
}

/// `N>&M-` moves the descriptor and leaves M closed.
#[test]
fn fd_move_closes_the_source() {
    expect(
        r##"exec 3>out.txt; exec 4>&3-; echo moved >&4; exec 4>&-; cat out.txt; { echo x >&3; } 2>/dev/null || echo fd3-closed; echo inp > i.txt; exec 5<i.txt; exec 6<&5-; read -r l <&6; echo $l
echo moved2 3>out2.txt 4>&3- >&4; cat out2.txt"##,
        r##"moved
fd3-closed
inp
moved2
"##,
        0,
    );
}

/// Writing to a closed stdout is an error, as in bash.
#[test]
fn closed_stdout_makes_echo_fail() {
    expect(
        r##"echo hi >&- 2>/dev/null; echo st=$?; (echo x >&-) 2>/dev/null; echo st=$?"##,
        r##"st=1
st=1
"##,
        0,
    );
}

/// Menu and prompt on stderr, invalid choices select nothing, EOF ends with status 1.
#[test]
fn select_reads_replies_like_bash() {
    expect(
        r##"printf '1\n\n3\nfoo\n2\n' | { select x in a b; do echo "[$x] [$REPLY]"; done; echo st=$?; } 2>&1"##,
        r##"1) a
2) b
#? [a] [1]
#? 1) a
2) b
#? [] [3]
#? [] [foo]
#? [b] [2]
#? 
st=1
"##,
        0,
    );
}

/// Long menus use columns across $COLUMNS.
#[test]
fn select_lays_out_columns() {
    expect(
        r##"printf '1\n' | { PS3='> '; select x in a b c d e f g h i j k l; do echo $x; break; done; } 2>&1"##,
        r##"1) a    3) c   5) e   7) g   9) i  11) k
2) b    4) d   6) f   8) h  10) j  12) l
> a
"##,
        0,
    );
}

/// Unnamed and named coprocesses, their descriptor arrays and PIDs.
#[test]
fn coproc_simple_and_named() {
    expect(
        r##"coproc cat; echo hi >&${COPROC[1]}; exec {COPROC[1]}>&-; read -r l <&${COPROC[0]}; echo $l; echo ${#COPROC[@]} ${COPROC_PID:+pid}
coproc UP { tr a-z A-Z; }; echo abc >&${UP[1]}; exec {UP[1]}>&-; read -r l <&${UP[0]}; echo $l; wait $UP_PID; echo st=$?"##,
        r##"hi
2 pid
ABC
st=0
"##,
        0,
    );
}

/// `local -` makes `set` options local to the function.
#[test]
fn local_dash_restores_set_options() {
    expect(
        r##"f() { local -; set -e; set -f; echo *; }; touch zz; f; [[ $- == *e* ]] && echo leaked || echo restored; echo *; local - 2>/dev/null; echo st=$?"##,
        r##"*
restored
zz
st=1
"##,
        0,
    );
}

/// The handler gets the command and arguments, in a separate environment.
#[test]
fn command_not_found_handle_runs_in_a_child() {
    expect(
        r##"command_not_found_handle() { x=changed; echo "cnf: $*"; return 3; }; x=orig; nosuch_zz a b; echo "st=$? $x"; ./nosuch_zz 2>/dev/null; echo st=$?"##,
        r##"cnf: nosuch_zz a b
st=3 orig
st=127
"##,
        0,
    );
}
