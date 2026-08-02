//! `JSH_HELPER_<NAME>` tells jsh where a trusted helper actually lives.
//!
//! The automatic candidate list is short and absolute — `/usr/bin/bash`,
//! `/usr/bin/git`, `/usr/bin/notify-send` and a couple of siblings. That costs
//! nothing on a distribution that puts those tools there and everything on one
//! that does not: Nix, Homebrew-style prefixes and immutable-root images lose
//! the `.bashrc` import, the Git prompt and desktop notifications with no way to
//! say where the tools are.
//!
//! The override is deliberately not a return to `PATH` lookup. `PATH` is
//! mutable shell state that any sourced script can rewrite; this is one
//! explicit absolute path that still has to survive the same trust checks, all
//! the way up its directory chain.
//!
//! Every test drives the bash helper through `source`, which is the one helper
//! path a non-interactive shell reaches. jsh only shells out when its own
//! parser cannot read the file, so each fixture opens with a construct that
//! parser rejects; the marker variable that comes back is the proof that bash
//! really ran it.

use std::fs;
use std::os::unix::fs::PermissionsExt;
use std::path::Path;
use std::process::{Command, Output};

/// Rejected by jsh's parser, so `source` falls through to the bash helper.
const FORCE_BASH: &str = "coproc helper_probe { true; }\n";

struct Fixture {
    _temp: tempfile::TempDir,
    script: std::path::PathBuf,
}

fn fixture(marker: &str) -> Fixture {
    let temp = tempfile::tempdir().expect("tempdir");
    let script = temp.path().join("fixture.sh");
    fs::write(&script, format!("{FORCE_BASH}export MARKER={marker}\n")).expect("fixture");
    Fixture {
        _temp: temp,
        script,
    }
}

fn source_with_helper(script: &Path, helper: Option<&str>) -> Output {
    let mut command = Command::new(env!("CARGO_BIN_EXE_jsh"));
    command.args([
        "--norc",
        "-c",
        &format!("source {}; echo \"MARKER=${{MARKER}}\"", script.display()),
    ]);
    command.env_remove("JSH_HELPER_BASH");
    if let Some(value) = helper {
        command.env("JSH_HELPER_BASH", value);
    }
    command.output().expect("run jsh")
}

fn stderr_of(output: &Output) -> String {
    String::from_utf8_lossy(&output.stderr).into_owned()
}

fn stdout_of(output: &Output) -> String {
    String::from_utf8_lossy(&output.stdout).into_owned()
}

#[test]
fn without_an_override_the_automatic_helper_still_runs() {
    // Baseline. If this cannot shell out to bash on this machine there is no
    // automatic helper here, and the override tests below have nothing to
    // contrast against.
    let fixture = fixture("automatic");
    let output = source_with_helper(&fixture.script, None);
    if !stdout_of(&output).contains("MARKER=automatic") {
        return;
    }
    assert!(!stderr_of(&output).contains("not a trusted executable"));
}

#[test]
fn a_trustworthy_override_is_used() {
    let Some(bash) = ["/usr/bin/bash", "/bin/bash"]
        .into_iter()
        .find(|candidate| Path::new(candidate).exists())
    else {
        return;
    };
    let fixture = fixture("viaoverride");
    let output = source_with_helper(&fixture.script, Some(bash));

    assert!(
        stdout_of(&output).contains("MARKER=viaoverride"),
        "the override did not run the script: stdout={:?} stderr={:?}",
        stdout_of(&output),
        stderr_of(&output)
    );
    assert!(
        !stderr_of(&output).contains("not a trusted executable"),
        "stderr={:?}",
        stderr_of(&output)
    );
}

#[test]
fn an_override_that_is_not_trustworthy_disables_the_integration() {
    let fixture = fixture("shouldnotappear");
    let output = source_with_helper(&fixture.script, Some("/no/such/bash"));
    let stderr = stderr_of(&output);

    assert!(
        stderr.contains("JSH_HELPER_BASH"),
        "the rejection must name the variable that caused it: {stderr:?}"
    );
    assert!(
        stderr.contains("not a trusted executable"),
        "stderr={stderr:?}"
    );
    // Silently starting a *different* bash than the one that was named would be
    // worse than the feature being missing: the operator asked for a specific
    // binary, and a system one is not it.
    assert!(
        stdout_of(&output).contains("MARKER=") && !stdout_of(&output).contains("shouldnotappear"),
        "a rejected override fell back to the automatic helper: stdout={:?}",
        stdout_of(&output)
    );
}

#[test]
fn the_rejection_is_reported_once_rather_than_per_use() {
    // Resolution happens from a prompt callback and a notification thread as
    // well as here, so an unconditional warning would repeat on every prompt.
    let fixture = fixture("noise");
    let mut command = Command::new(env!("CARGO_BIN_EXE_jsh"));
    let script = fixture.script.display();
    command.args([
        "--norc",
        "-c",
        &format!("source {script}; source {script}; source {script}"),
    ]);
    command.env("JSH_HELPER_BASH", "/no/such/bash");
    let output = command.output().expect("run jsh");

    let warnings = stderr_of(&output)
        .lines()
        .filter(|line| line.contains("not a trusted executable"))
        .count();
    assert_eq!(warnings, 1, "stderr={:?}", stderr_of(&output));
}

#[test]
fn an_override_under_a_world_writable_directory_is_refused() {
    // The whole reason the directory chain is walked. The wrapper is 0700
    // inside a 0700 directory and would pass a check that looked only at the
    // leaf — but it lives under a sticky-bit /tmp that anyone can plant names
    // in, so the directory holding it can be renamed away and replaced.
    let fixture = fixture("viawrapper");
    let directory = fixture.script.parent().expect("fixture directory");
    fs::set_permissions(directory, fs::Permissions::from_mode(0o700)).expect("private dir");
    let wrapper = directory.join("bash");
    fs::write(&wrapper, "#!/bin/sh\nexec /bin/bash \"$@\"\n").expect("wrapper");
    fs::set_permissions(&wrapper, fs::Permissions::from_mode(0o700)).expect("executable");

    let world_writable_ancestor = directory.ancestors().any(|ancestor| {
        fs::metadata(ancestor).is_ok_and(|metadata| metadata.permissions().mode() & 0o022 != 0)
    });
    if !world_writable_ancestor {
        // TMPDIR is somewhere private on this machine; nothing to prove.
        return;
    }

    let output = source_with_helper(
        &fixture.script,
        Some(wrapper.to_str().expect("utf-8 wrapper path")),
    );
    assert!(
        stderr_of(&output).contains("not a trusted executable"),
        "a helper under a world-writable ancestor was accepted: {:?}",
        stderr_of(&output)
    );
}

#[test]
fn an_empty_override_is_ignored_rather_than_treated_as_a_path() {
    let fixture = fixture("empty");
    let output = source_with_helper(&fixture.script, Some(""));
    assert!(
        !stderr_of(&output).contains("not a trusted executable"),
        "an override that is merely empty should not be reported: {:?}",
        stderr_of(&output)
    );
}
