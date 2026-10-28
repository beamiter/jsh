//! Exercise the actual installed-binary-style embedded controller entry point.
use std::process::Command;

#[test]
fn research_help_works_without_a_source_checkout() {
    let directory = tempfile::tempdir().unwrap();
    let output = Command::new(env!("CARGO_BIN_EXE_jsh"))
        .current_dir(directory.path())
        .args(["research", "--help"])
        .output()
        .unwrap();
    assert!(output.status.success(), "{:?}", output);
    let stdout = String::from_utf8(output.stdout).unwrap();
    assert!(stdout.contains("jsh research"));
    assert!(stdout.contains("enqueue"));
    assert!(stdout.contains("recover"));
}

#[test]
fn research_does_not_execute_without_explicit_authorization() {
    let directory = tempfile::tempdir().unwrap();
    let output = Command::new(env!("CARGO_BIN_EXE_jsh"))
        .current_dir(directory.path())
        .args(["research", "run"])
        .output()
        .unwrap();
    assert_eq!(output.status.code(), Some(2));
    let stderr = String::from_utf8(output.stderr).unwrap();
    assert!(stderr.contains("--execute"));
    assert!(!directory.path().join(".jsh-research").exists());
}

#[test]
fn research_unknown_commands_fail_closed() {
    let output = Command::new(env!("CARGO_BIN_EXE_jsh"))
        .args(["research", "submit-dlc"])
        .output()
        .unwrap();
    assert_eq!(output.status.code(), Some(2));
}
