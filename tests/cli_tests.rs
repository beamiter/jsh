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
    assert!(text(&help.stdout).contains("doctor [--json] [--strict]"));

    let version = run(&["--version"]);
    assert!(version.status.success());
    assert_eq!(text(&version.stdout).trim(), "jsh 0.2.0");
}

#[test]
fn doctor_supports_human_and_machine_readable_diagnostics() {
    let human = run(&["doctor"]);
    assert!(human.status.success(), "{}", text(&human.stderr));
    assert!(text(&human.stdout).contains("jsh doctor 0.2.0"));
    assert!(text(&human.stdout).contains("Summary:"));

    let machine = run(&["doctor", "--json"]);
    assert!(machine.status.success(), "{}", text(&machine.stderr));
    let report: serde_json::Value =
        serde_json::from_slice(&machine.stdout).expect("doctor JSON report");
    assert_eq!(report["ok"], true);
    assert_eq!(report["schema_version"], 1);
    assert_eq!(report["kind"], "doctor");
    assert!(report["healthy"].is_boolean());
    assert!(report["checks"]
        .as_array()
        .is_some_and(|checks| !checks.is_empty()));

    let invalid = run(&["doctor", "--json", "--unknown"]);
    assert_eq!(invalid.status.code(), Some(2));
    let error: serde_json::Value =
        serde_json::from_slice(&invalid.stderr).expect("doctor JSON error");
    assert_eq!(error["error"]["kind"], "usage");
}

#[test]
fn doctor_strict_and_rcfile_are_ci_friendly() {
    let missing = run(&[
        "doctor",
        "--strict",
        "--json",
        "--rcfile=/definitely/missing/jsh-doctor-rc",
    ]);
    assert_eq!(missing.status.code(), Some(1));
    let report: serde_json::Value =
        serde_json::from_slice(&missing.stdout).expect("strict doctor JSON");
    assert_eq!(report["healthy"], false);
    assert!(report["summary"]["warnings"].as_u64().unwrap_or(0) >= 1);

    let home = tempfile::tempdir().expect("doctor home");
    let rcfile = home.path().join("custom.jsh");
    std::fs::write(&rcfile, "export JSH_DOCTOR_FIXTURE=1\n").expect("write rcfile");
    let output = Command::new(env!("CARGO_BIN_EXE_jsh"))
        .env("HOME", home.path())
        .env("XDG_STATE_HOME", home.path().join("state"))
        .args([
            "doctor",
            "--strict",
            "--json",
            &format!("--rcfile={}", rcfile.display()),
        ])
        .output()
        .expect("diagnose custom rcfile");
    assert!(output.status.success(), "{}", text(&output.stderr));
    let report: serde_json::Value =
        serde_json::from_slice(&output.stdout).expect("custom rcfile report");
    assert!(report["checks"].as_array().unwrap().iter().any(|check| {
        check["name"] == "startup.file"
            && check["level"] == "pass"
            && check["message"]
                .as_str()
                .is_some_and(|message| message.contains("startup file is readable and bounded"))
    }));
}

#[cfg(unix)]
#[test]
fn doctor_reports_unsafe_persistence_entries_without_following_them() {
    use std::os::unix::fs::symlink;

    let home = tempfile::tempdir().expect("doctor home");
    let victim = home.path().join("victim");
    std::fs::write(&victim, "must remain untouched").expect("victim");
    symlink(&victim, home.path().join(".jsh_history")).expect("history symlink");

    let output = Command::new(env!("CARGO_BIN_EXE_jsh"))
        .env("HOME", home.path())
        .env("XDG_STATE_HOME", home.path().join("state"))
        .args(["doctor", "--strict", "--json"])
        .output()
        .expect("diagnose unsafe persistence");
    assert_eq!(output.status.code(), Some(1));
    assert_eq!(
        std::fs::read_to_string(&victim).unwrap(),
        "must remain untouched"
    );
    let report: serde_json::Value =
        serde_json::from_slice(&output.stdout).expect("persistence report");
    assert!(report["checks"].as_array().unwrap().iter().any(|check| {
        check["name"] == "persistence.integrity"
            && check["level"] == "warn"
            && check["message"]
                .as_str()
                .is_some_and(|message| message.contains(".jsh_history is a symlink"))
    }));
}

#[test]
fn long_options_accept_equals_and_reject_duplicate_single_values() {
    let output = run(&[
        "--command=printf '%s|%s\\n' \"$0\" \"$1\"",
        "worker",
        "value",
    ]);
    assert!(output.status.success(), "{}", text(&output.stderr));
    assert_eq!(text(&output.stdout).trim(), "worker|value");

    for args in [
        vec!["--rcfile=one", "--rcfile", "two"],
        vec!["--session=one", "--session", "two"],
    ] {
        let output = run(&args);
        assert_eq!(output.status.code(), Some(2));
        assert!(text(&output.stderr).contains("may only be specified once"));
    }
}

#[test]
fn doctor_json_never_echoes_credentials_or_helper_side_channel_warnings() {
    let output = Command::new(env!("CARGO_BIN_EXE_jsh"))
        .args(["doctor", "--json"])
        .env("JSH_AI_PROVIDER", "openai")
        .env("OPENAI_API_KEY", "jsh-doctor-secret-must-not-leak")
        .env("JSH_HELPER_BASH", "/not/a/trusted/bash")
        .output()
        .expect("run doctor with invalid environment");
    assert!(output.status.success());
    assert!(output.stderr.is_empty(), "{}", text(&output.stderr));
    let report = text(&output.stdout);
    assert!(!report.contains("jsh-doctor-secret-must-not-leak"));
    assert!(!report.contains("/not/a/trusted/bash"));
    let value: serde_json::Value = serde_json::from_str(&report).expect("doctor JSON report");
    assert_eq!(value["kind"], "doctor");
    assert!(value["summary"]["warnings"].as_u64().unwrap_or(0) >= 1);
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
fn noninteractive_cd_does_not_mutate_the_frecency_database() {
    let home = tempfile::tempdir().expect("temp home");
    let output = Command::new(env!("CARGO_BIN_EXE_jsh"))
        .env("HOME", home.path())
        .args(["-c", "cd /; pwd"])
        .output()
        .expect("run noninteractive cd");
    assert!(output.status.success(), "{}", text(&output.stderr));
    assert_eq!(text(&output.stdout).trim(), "/");
    assert!(!home.path().join(".jsh_z").exists());
}

#[test]
fn explicit_exit_status_reaches_the_parent_process() {
    let output = run_stdin(&[], "exit 7\n");
    assert_eq!(output.status.code(), Some(7));
}
