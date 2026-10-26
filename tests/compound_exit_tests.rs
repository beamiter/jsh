use std::fs::{self, File};
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

fn check(script: &str, expected_status: i32, expected_stdout: &str) {
    let temp = tempfile::tempdir().unwrap();
    let stdout_path = temp.path().join("stdout");
    let stderr_path = temp.path().join("stderr");
    let mut child = Command::new(env!("CARGO_BIN_EXE_jsh"))
        .args(["--norc", "-c", script])
        .env("HOME", temp.path())
        .current_dir(temp.path())
        .stdin(Stdio::null())
        .stdout(File::create(&stdout_path).unwrap())
        .stderr(File::create(&stderr_path).unwrap())
        .spawn()
        .expect("spawn isolated shell");
    let deadline = Instant::now() + Duration::from_secs(3);
    let status = loop {
        if let Some(status) = child.try_wait().unwrap() {
            break status;
        }
        if Instant::now() >= deadline {
            let _ = child.kill();
            let _ = child.wait();
            panic!("compound execution did not terminate: {script}");
        }
        std::thread::sleep(Duration::from_millis(5));
    };
    let stdout = fs::read_to_string(stdout_path).unwrap();
    let stderr = fs::read_to_string(stderr_path).unwrap();
    assert_eq!(status.code(), Some(expected_status), "{script}\n{stderr}");
    assert_eq!(stdout, expected_stdout, "{script}\n{stderr}");
}

#[test]
fn errexit_stops_each_loop_after_its_first_unhandled_failure() {
    for body in [
        "for x in 1 2; do echo $x; false; done",
        "for ((x=1; x<=2; x++)); do echo $x; false; done",
        "x=0; while [ $x -lt 2 ]; do x=$((x+1)); echo $x; false; done",
        "x=0; until [ $x -ge 2 ]; do x=$((x+1)); echo $x; false; done",
    ] {
        check(&format!("set -e; {body}; echo unreachable"), 1, "1\n");
    }
}

#[test]
fn compound_final_status_preserves_and_or_failure_exemptions() {
    for body in [
        "{ false && echo unreachable; }",
        "if true; then false && echo unreachable; fi",
        "for x in 1 2; do false && echo unreachable; done",
    ] {
        check(&format!("set -e; {body}; echo reached"), 0, "reached\n");
    }
}

#[test]
fn tested_loops_and_functions_keep_errexit_suppressed_through_their_bodies() {
    check(
        "set -e; for x in 1 2; do echo $x; false; done || echo caught; echo reached",
        0,
        "1\n2\ncaught\nreached\n",
    );
    check(
        "set -e; f() { false; echo survived; }; if f; then echo reached; fi",
        0,
        "survived\nreached\n",
    );
    // A function call is a fresh command boundary, unlike a brace group.
    check(
        "set -e; f() { false && echo unreachable; }; f; echo unreachable",
        1,
        "",
    );
}

#[test]
fn failed_conditions_are_exempt_but_selected_bodies_are_not() {
    check(
        "set -e; if false; then echo unreachable; else echo selected; fi; \
         while false; do echo unreachable; done; \
         until true; do echo unreachable; done; echo reached",
        0,
        "selected\nreached\n",
    );
    check(
        "set -e; if true; then false; echo unreachable; fi; echo unreachable",
        1,
        "",
    );
}

#[test]
fn until_condition_break_is_consumed_by_its_own_loop() {
    check(
        "for x in 1 2; do until break; do echo unreachable; done; echo $x; done; echo reached",
        0,
        "1\n2\nreached\n",
    );
}

#[test]
fn until_condition_continue_does_not_escape_after_the_condition_succeeds() {
    check(
        "x=0; until x=$((x+1)); if [ $x -lt 3 ]; then continue; fi; true; \
         do echo unreachable; done; echo x=$x",
        0,
        "x=1\n",
    );
}

#[test]
fn exit_and_return_from_condition_lists_stop_the_containing_execution() {
    check(
        "if exit 7; then echo unreachable; else echo unreachable; fi; echo unreachable",
        7,
        "",
    );
    for condition in [
        "while return 9; do echo unreachable; done",
        "until return 9; do echo unreachable; done",
        "if return 9; then echo unreachable; else echo unreachable; fi",
    ] {
        check(
            &format!("f() {{ {condition}; echo unreachable; }}; f; echo status=$?"),
            0,
            "status=9\n",
        );
    }
}

#[test]
fn err_trap_runs_once_before_errexit_unwinds_a_loop() {
    check(
        "set -e; trap 'echo ERR:$?' ERR; for x in 1 2; do echo $x; false; done; echo unreachable",
        1,
        "1\nERR:1\n",
    );
    check(
        "set -e; trap 'set +e; echo handled' ERR; for x in 1; do false; done; echo reached",
        0,
        "handled\nreached\n",
    );
    check(
        "set -e; trap 'echo EXIT:$?' EXIT; for x in 1 2; do false; done",
        1,
        "EXIT:1\n",
    );
}

#[test]
fn eval_propagates_errexit_exit_and_function_return() {
    check(
        "set -e; eval 'false; echo unreachable'; echo unreachable",
        1,
        "",
    );
    check("eval 'exit 8; echo unreachable'; echo unreachable", 8, "");
    check(
        "f() { eval 'return 7; echo unreachable'; echo unreachable; }; f; echo status=$?",
        0,
        "status=7\n",
    );
    check(
        "set -e; if eval 'false; echo survived'; then echo reached; fi",
        0,
        "survived\nreached\n",
    );
}

#[test]
fn sourced_errexit_unwinds_the_calling_program_and_keeps_conditional_exemptions() {
    check(
        "printf '%s\\n' 'for x in 1 2; do echo $x; false; done' 'echo unreachable' >source.jsh; \
         set -e; source source.jsh; echo unreachable",
        1,
        "1\n",
    );
    check(
        "printf '%s\\n' 'false' 'echo survived' >source.jsh; \
         set -e; if source source.jsh; then echo reached; fi",
        0,
        "survived\nreached\n",
    );
}
