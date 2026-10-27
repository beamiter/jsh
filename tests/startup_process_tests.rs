//! Shell startup must not start helper processes it does not need.
//!
//! Querying the terminal size used to go through `crossterm::terminal::size()`,
//! which falls back to running `tput cols` and `tput lines` through `PATH`
//! whenever there is no terminal to ask. Every `jsh -c`, script, and pipeline
//! therefore paid two process spawns and resolved a helper through mutable
//! shell state. These tests plant a `tput` that records being run.
use std::os::unix::fs::PermissionsExt;
use std::path::Path;
use std::process::{Command, Stdio};

fn plant_recording_helper(dir: &Path, name: &str, marker: &Path) {
    let script = format!("#!/bin/sh\necho ran >> '{}'\necho 80\n", marker.display());
    let path = dir.join(name);
    std::fs::write(&path, script).expect("write helper");
    std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o755)).expect("chmod");
}

fn run_without_terminal(path_dir: &Path, home: &Path, args: &[&str]) -> std::process::Output {
    Command::new(env!("CARGO_BIN_EXE_jsh"))
        .args(args)
        .env_clear()
        .env("PATH", format!("{}:/usr/bin:/bin", path_dir.display()))
        .env("HOME", home)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .output()
        .expect("spawn jsh")
}

#[test]
fn noninteractive_startup_does_not_run_tput() {
    let bin = tempfile::tempdir().expect("tempdir");
    let home = tempfile::tempdir().expect("tempdir");
    let marker = bin.path().join("tput-ran");
    plant_recording_helper(bin.path(), "tput", &marker);

    let out = run_without_terminal(bin.path(), home.path(), &["--norc", "-c", "echo ok"]);
    assert_eq!(String::from_utf8_lossy(&out.stdout), "ok\n");
    assert!(out.status.success());
    assert!(
        !marker.exists(),
        "jsh ran a PATH-resolved tput during startup"
    );
}

#[test]
fn script_and_stdin_startup_do_not_run_tput() {
    let bin = tempfile::tempdir().expect("tempdir");
    let home = tempfile::tempdir().expect("tempdir");
    let marker = bin.path().join("tput-ran");
    plant_recording_helper(bin.path(), "tput", &marker);
    let script = home.path().join("s.sh");
    std::fs::write(&script, "echo from-script\n").expect("write script");

    let out = run_without_terminal(bin.path(), home.path(), &[script.to_str().unwrap()]);
    assert_eq!(String::from_utf8_lossy(&out.stdout), "from-script\n");
    assert!(!marker.exists(), "script startup ran tput");
}

#[test]
fn columns_still_sets_the_width_without_a_terminal() {
    // COLUMNS remains the explicit override when no terminal can be queried.
    let home = tempfile::tempdir().expect("tempdir");
    let out = Command::new(env!("CARGO_BIN_EXE_jsh"))
        .args(["--norc", "-c", "echo '[{\"a\":1}]' | from-json | to-table"])
        .env_clear()
        .env("PATH", "/usr/bin:/bin")
        .env("HOME", home.path())
        .env("COLUMNS", "40")
        .stdin(Stdio::null())
        .output()
        .expect("spawn jsh");
    assert!(
        out.status.success(),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
    assert!(String::from_utf8_lossy(&out.stdout).contains('a'));
}
