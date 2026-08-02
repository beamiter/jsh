//! `JSH_REAL_HOME` separates "the home a person types" from "the home this
//! shell writes to".
//!
//! `jsh-remote.sh --incognito` points `HOME` at a sandbox so that nothing jsh
//! writes survives the session, which would otherwise also move `~`, `cd` with
//! no argument, and the prompt's `~/…` abbreviation into that sandbox. The
//! override moves those back without moving any state with them.
//!
//! Every test points both homes at its own tempdir, so nothing here can read or
//! write the developer's shell data.

use std::fs;
use std::path::Path;
use std::process::{Command, Output};

fn jsh(sandbox: &Path, real: Option<&Path>, script: &str) -> Output {
    let mut command = Command::new(env!("CARGO_BIN_EXE_jsh"));
    command
        .args(["--norc", "-c", script])
        .env("HOME", sandbox)
        .env("XDG_STATE_HOME", sandbox.join("state"))
        .env("XDG_CACHE_HOME", sandbox.join("cache"))
        .env_remove("JSH_REAL_HOME");
    if let Some(real) = real {
        command.env("JSH_REAL_HOME", real);
    }
    command.output().expect("run jsh")
}

fn stdout_of(output: &Output) -> String {
    String::from_utf8_lossy(&output.stdout).trim().to_string()
}

struct Homes {
    _temp: tempfile::TempDir,
    sandbox: std::path::PathBuf,
    real: std::path::PathBuf,
}

fn homes() -> Homes {
    let temp = tempfile::tempdir().expect("tempdir");
    let sandbox = temp.path().join("sandbox");
    let real = temp.path().join("real");
    fs::create_dir_all(&sandbox).expect("sandbox");
    fs::create_dir_all(&real).expect("real home");
    Homes {
        _temp: temp,
        sandbox,
        real,
    }
}

#[test]
fn tilde_follows_the_override_while_home_stays_the_sandbox() {
    let h = homes();
    let out = jsh(&h.sandbox, Some(&h.real), "echo ~; echo \"$HOME\"");
    let lines: Vec<String> = stdout_of(&out).lines().map(str::to_string).collect();

    assert_eq!(lines.len(), 2, "unexpected output: {lines:?}");
    // `~` is what the person typed, so it names the account's real home.
    assert_eq!(lines[0], h.real.to_string_lossy());
    // `$HOME` is where programs write, so it stays the sandbox. The two
    // deliberately disagree; that disagreement is the whole feature.
    assert_eq!(lines[1], h.sandbox.to_string_lossy());
}

#[test]
fn cd_with_no_argument_lands_in_the_real_home() {
    let h = homes();
    let out = jsh(&h.sandbox, Some(&h.real), "cd; pwd");
    assert_eq!(stdout_of(&out), h.real.to_string_lossy());
}

#[test]
fn tilde_prefixed_paths_resolve_against_the_real_home() {
    let h = homes();
    fs::write(h.real.join("marker"), "real\n").expect("marker");
    let out = jsh(&h.sandbox, Some(&h.real), "cat ~/marker");
    assert_eq!(stdout_of(&out), "real");
}

#[test]
fn history_still_belongs_to_the_sandbox() {
    // The point of the sandbox is that jsh's own state cannot escape it. An
    // override that also moved history would quietly write a shared account's
    // real home, which is exactly what --incognito exists to prevent.
    let h = homes();
    let out = jsh(&h.sandbox, Some(&h.real), "echo hi");
    assert!(out.status.success(), "jsh failed: {out:?}");

    let escaped: Vec<_> = fs::read_dir(&h.real)
        .expect("real home")
        .filter_map(Result::ok)
        .map(|entry| entry.file_name().to_string_lossy().to_string())
        .filter(|name| name.starts_with(".jsh"))
        .collect();
    assert!(
        escaped.is_empty(),
        "jsh state escaped into the real home: {escaped:?}"
    );
}

#[test]
fn an_unusable_override_is_ignored_rather_than_fatal() {
    let h = homes();
    let missing = h.real.join("does-not-exist");

    let mut command = Command::new(env!("CARGO_BIN_EXE_jsh"));
    let out = command
        .args(["--norc", "-c", "echo ~"])
        .env("HOME", &h.sandbox)
        .env("XDG_STATE_HOME", h.sandbox.join("state"))
        .env("JSH_REAL_HOME", &missing)
        .output()
        .expect("run jsh");

    assert!(out.status.success(), "jsh failed: {out:?}");
    assert_eq!(stdout_of(&out), h.sandbox.to_string_lossy());
}

#[test]
fn without_the_override_nothing_changes() {
    let h = homes();
    let out = jsh(&h.sandbox, None, "echo ~; cd; pwd");
    let lines: Vec<String> = stdout_of(&out).lines().map(str::to_string).collect();
    assert_eq!(lines.len(), 2, "unexpected output: {lines:?}");
    assert_eq!(lines[0], h.sandbox.to_string_lossy());
    assert_eq!(lines[1], h.sandbox.to_string_lossy());
}
