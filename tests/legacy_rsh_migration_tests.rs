//! Startup migration of pre-rename `rsh` user data.
//!
//! The unit tests in src/config.rs drive the migration directly; these spawn
//! the real binary so the startup wiring is covered too. Every test points HOME
//! and XDG_STATE_HOME at its own tempdir, so nothing here can read or write the
//! developer's shell data.

use std::fs;
use std::os::unix::fs::PermissionsExt;
use std::path::Path;
use std::process::{Command, Output};

const OLD_HISTORY: &str = concat!(
    r#"{"rsh_history_version":1,"command":"echo one","timestamp":10,"cwd":"/p"}"#,
    "\n",
    r#"{"rsh_history_version":1,"command":"cargo build --release","timestamp":11,"cwd":"/p"}"#,
    "\n",
    r#"{"rsh_history_version":1,"command":"if true\nthen echo hi\nfi","timestamp":12,"cwd":"/p"}"#,
    "\n"
);

const OLD_JOURNAL: &str = concat!(
    r#"{"rsh_execution_version":1,"event":"start","id":"rsh-a","session_id":null,"seq":1,"command":"make","cwd":"/p","started_at_ms":10}"#,
    "\n",
    r#"{"rsh_execution_version":1,"event":"finish","id":"rsh-a","exit_code":2,"duration_ms":5,"cwd_after":"/p","ended_at_ms":15}"#,
    "\n"
);

fn jsh_in(home: &Path, state: &Path, args: &[&str]) -> Output {
    Command::new(env!("CARGO_BIN_EXE_jsh"))
        .args(args)
        .env("HOME", home)
        .env("XDG_STATE_HOME", state)
        .output()
        .expect("run jsh")
}

fn text(bytes: &[u8]) -> String {
    String::from_utf8_lossy(bytes).into_owned()
}

fn write_legacy_home(root: &Path) -> (std::path::PathBuf, std::path::PathBuf) {
    let home = root.join("home");
    let state = root.join("state");
    fs::create_dir_all(&home).expect("home");
    fs::set_permissions(&home, fs::Permissions::from_mode(0o700)).expect("private home");
    fs::create_dir_all(state.join("rsh")).expect("legacy state dir");
    fs::write(home.join(".rsh_history"), OLD_HISTORY).expect("legacy history");
    fs::write(home.join(".rsh_bookmarks"), "proj|/p\ndl|/d\n").expect("legacy bookmarks");
    fs::write(state.join("rsh").join("executions.jsonl"), OLD_JOURNAL).expect("legacy journal");
    (home, state)
}

#[test]
fn a_pre_rename_home_keeps_its_history_and_bookmarks() {
    let temp = tempfile::tempdir().expect("tempdir");
    let (home, state) = write_legacy_home(temp.path());

    let output = jsh_in(&home, &state, &["-c", "history; bookmark ls"]);

    assert!(output.status.success(), "{}", text(&output.stderr));
    let stdout = text(&output.stdout);
    // Every record survives, multi-line commands included.
    assert!(stdout.contains("echo one"), "{stdout}");
    assert!(stdout.contains("cargo build --release"), "{stdout}");
    assert!(stdout.contains("if true\nthen echo hi\nfi"), "{stdout}");
    assert!(stdout.contains("proj"), "{stdout}");
    assert!(stdout.contains("dl"), "{stdout}");
    // One info line naming what moved, so a bug report shows it.
    let stderr = text(&output.stderr);
    assert!(stderr.contains("migrated"), "{stderr}");
    assert!(stderr.contains(".rsh_history"), "{stderr}");
    // The rsh files are still there for an installed rsh binary.
    assert_eq!(
        fs::read_to_string(home.join(".rsh_history")).expect("legacy history"),
        OLD_HISTORY
    );
}

#[test]
fn a_migrated_journal_still_answers_context_queries() {
    let temp = tempfile::tempdir().expect("tempdir");
    let (home, state) = write_legacy_home(temp.path());

    let output = jsh_in(&home, &state, &["context", "last-failed"]);

    assert!(output.status.success(), "{}", text(&output.stderr));
    let stdout = text(&output.stdout);
    assert!(stdout.contains("rsh-a"), "{stdout}");
    assert!(stdout.contains("make"), "{stdout}");
    assert!(state.join("jsh").join("executions.jsonl").is_file());
}

#[test]
fn a_second_start_migrates_nothing_and_says_nothing() {
    let temp = tempfile::tempdir().expect("tempdir");
    let (home, state) = write_legacy_home(temp.path());

    let first = jsh_in(&home, &state, &["-c", "history"]);
    assert!(text(&first.stderr).contains("migrated"));
    let migrated = fs::read_to_string(home.join(".jsh_history")).expect("new history");

    let second = jsh_in(&home, &state, &["-c", "history"]);

    assert!(second.status.success());
    // A script's stderr stays clean on every later run.
    assert_eq!(text(&second.stderr), "");
    assert_eq!(
        fs::read_to_string(home.join(".jsh_history")).expect("new history"),
        migrated
    );
}

#[test]
fn existing_jsh_history_is_never_replaced_by_the_rsh_one() {
    let temp = tempfile::tempdir().expect("tempdir");
    let (home, state) = write_legacy_home(temp.path());
    let current = String::from(
        r#"{"jsh_history_version":1,"command":"only mine","timestamp":99,"cwd":null}"#,
    ) + "\n";
    fs::write(home.join(".jsh_history"), &current).expect("current history");

    let output = jsh_in(&home, &state, &["-c", "history"]);

    assert!(output.status.success(), "{}", text(&output.stderr));
    let stdout = text(&output.stdout);
    assert!(stdout.contains("only mine"), "{stdout}");
    assert!(!stdout.contains("cargo build --release"), "{stdout}");
    assert_eq!(
        fs::read_to_string(home.join(".jsh_history")).expect("current history"),
        current
    );
}

#[test]
fn a_corrupt_legacy_file_does_not_stop_startup() {
    let temp = tempfile::tempdir().expect("tempdir");
    let (home, state) = write_legacy_home(temp.path());
    fs::write(home.join(".rsh_history"), b"\x00\x01 not history \xff\n").expect("corrupt history");
    fs::write(state.join("rsh").join("executions.jsonl"), "}{ not jsonl\n")
        .expect("corrupt journal");

    // `history` is what forces the migration to run: it is the first thing to
    // open a default data path.
    let output = jsh_in(&home, &state, &["-c", "history; echo alive"]);

    assert!(output.status.success(), "{}", text(&output.stderr));
    assert!(
        text(&output.stdout).ends_with("alive\n"),
        "{}",
        text(&output.stdout)
    );
    assert!(home.join(".jsh_history").is_file());
}

#[test]
fn an_unreadable_legacy_file_is_a_warning_not_a_failure() {
    let temp = tempfile::tempdir().expect("tempdir");
    let (home, state) = write_legacy_home(temp.path());
    fs::set_permissions(home.join(".rsh_history"), fs::Permissions::from_mode(0o000))
        .expect("make legacy history unreadable");

    let output = jsh_in(&home, &state, &["-c", "history; bookmark ls; echo alive"]);

    assert!(output.status.success(), "{}", text(&output.stderr));
    assert!(
        text(&output.stdout).ends_with("alive\n"),
        "{}",
        text(&output.stdout)
    );
    let stderr = text(&output.stderr);
    assert!(stderr.contains("warning"), "{stderr}");
    assert!(stderr.contains(".rsh_history"), "{stderr}");
    assert!(!home.join(".jsh_history").exists());
    // Unrelated data still made it across.
    assert!(home.join(".jsh_bookmarks").is_file());
}
