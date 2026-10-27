//! Signals sent to a freshly forked command must reach it.
//!
//! A forked child keeps the shell's signal handlers until it resets them and
//! execs its program. Those handlers only latch a flag, so a signal that
//! landed in that window (`sleep 5 & kill $!`) used to be absorbed and lost,
//! and the command ran to completion. Under load this lost roughly one signal
//! in twelve; the runs below are concurrent to reproduce that pressure.
use std::process::{Command, Stdio};

fn run(script: &str) -> String {
    let out = Command::new(env!("CARGO_BIN_EXE_jsh"))
        .args(["--norc", "-c", script])
        .stdin(Stdio::null())
        .stderr(Stdio::null())
        .output()
        .expect("spawn jsh");
    String::from_utf8_lossy(&out.stdout).into_owned()
}

#[test]
fn a_signal_sent_right_after_launch_is_not_lost() {
    let lost: Vec<String> = std::thread::scope(|scope| {
        let workers: Vec<_> = (0..8)
            .map(|_| {
                scope.spawn(|| {
                    (0..6)
                        .map(|_| run("sleep 5 & kill -s TERM $!; wait $!; echo st=$?"))
                        .filter(|out| out != "st=143\n")
                        .collect::<Vec<_>>()
                })
            })
            .collect();
        workers
            .into_iter()
            .flat_map(|worker| worker.join().expect("worker"))
            .collect()
    });
    assert!(lost.is_empty(), "signals lost: {lost:?}");
}

#[test]
fn job_spec_kill_reaches_a_fresh_background_job() {
    assert_eq!(run("sleep 5 & kill %1; wait $!; echo st=$?"), "st=143\n");
}
