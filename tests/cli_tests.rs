use std::io::Write;
use std::process::{Command, Output, Stdio};

fn jsh() -> Command {
    Command::new(env!("CARGO_BIN_EXE_jsh"))
}

fn run(args: &[&str]) -> Output {
    jsh().args(args).output().expect("run jsh")
}

fn run_stdin(args: &[&str], input: &str) -> Output {
    let mut child = jsh()
        .args(args)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .expect("spawn jsh");
    child
        .stdin
        .as_mut()
        .expect("stdin")
        .write_all(input.as_bytes())
        .expect("write stdin");
    child.wait_with_output().expect("wait for jsh")
}

fn text(bytes: &[u8]) -> String {
    String::from_utf8_lossy(bytes).into_owned()
}

#[test]
fn help_and_version_are_real_cli_actions() {
    let help = run(&["--help"]);
    assert!(help.status.success());
    assert!(text(&help.stdout).contains("Usage:"));
    assert!(text(&help.stdout).contains("--rcfile"));

    let version = run(&["--version"]);
    assert!(version.status.success());
    assert_eq!(text(&version.stdout).trim(), "jsh 0.2.0");
}

#[test]
fn malformed_cli_exits_two_with_a_diagnostic() {
    for args in [vec!["--unknown"], vec!["-c"], vec!["--rcfile"]] {
        let output = run(&args);
        assert_eq!(output.status.code(), Some(2), "args: {args:?}");
        assert!(!output.stderr.is_empty(), "args: {args:?}");
        assert!(text(&output.stderr).contains("jsh:"), "args: {args:?}");
    }
}

#[test]
fn command_mode_assigns_arg0_and_positionals_like_bash() {
    let output = run(&[
        "-c",
        "printf '%s|%s|%s|%s\\n' \"$0\" \"$1\" \"$2\" \"$#\"",
        "worker",
        "one",
        "two",
    ]);
    assert!(output.status.success(), "{}", text(&output.stderr));
    assert_eq!(text(&output.stdout).trim(), "worker|one|two|2");
}

#[test]
fn script_mode_uses_path_as_arg0() {
    let dir = tempfile::tempdir().expect("tempdir");
    let script = dir.path().join("args.jsh");
    std::fs::write(&script, "printf '%s|%s|%s\\n' \"$0\" \"$1\" \"$#\"\n").expect("write script");

    let output = Command::new(env!("CARGO_BIN_EXE_jsh"))
        .arg(&script)
        .arg("one")
        .output()
        .expect("run script");
    assert!(output.status.success(), "{}", text(&output.stderr));
    assert_eq!(
        text(&output.stdout).trim(),
        format!("{}|one|1", script.display())
    );
}

#[test]
fn syntax_check_does_not_execute() {
    let dir = tempfile::tempdir().expect("tempdir");
    let marker = dir.path().join("must-not-exist");
    let command = format!("echo touched > {}", marker.display());

    let output = run(&["--check", "-c", &command]);
    assert!(output.status.success(), "{}", text(&output.stderr));
    assert!(!marker.exists());

    let invalid = run(&["--check", "-c", "if true"]);
    assert_eq!(invalid.status.code(), Some(2));
    assert!(text(&invalid.stderr).contains("incomplete"));
}

#[test]
fn stdin_mode_propagates_status_and_skips_interactive_config() {
    let dir = tempfile::tempdir().expect("tempdir");
    std::fs::write(dir.path().join(".bashrc"), "export JSH_RC_WAS_LOADED=yes\n")
        .expect("write bashrc");

    let output = Command::new(env!("CARGO_BIN_EXE_jsh"))
        .env("HOME", dir.path())
        .arg("-s")
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .and_then(|mut child| {
            child
                .stdin
                .as_mut()
                .expect("stdin")
                .write_all(b"echo ${JSH_RC_WAS_LOADED:-no}; false\n")?;
            child.wait_with_output()
        })
        .expect("run stdin mode");

    assert_eq!(text(&output.stdout).trim(), "no");
    assert_eq!(output.status.code(), Some(1));
}

#[test]
fn explicit_exit_status_reaches_the_parent_process() {
    let output = run_stdin(&[], "exit 7\n");
    assert_eq!(output.status.code(), Some(7));
}
