//! Process-isolated redirection regressions. Bash is an independent oracle for
//! the supported shell forms; typed commands have direct jsh expectations.
use std::path::Path;
use std::process::{Command, Output};

fn run(binary: &str, cwd: &Path, script: &str) -> Output {
    Command::new(binary)
        .args(["-c", script])
        .current_dir(cwd)
        .env_remove("BASH_ENV")
        .output()
        .expect("run shell")
}

fn compare(script: &str, files: &[&str]) {
    let bash_dir = tempfile::tempdir().unwrap();
    let jsh_dir = tempfile::tempdir().unwrap();
    let expected = run("/bin/bash", bash_dir.path(), script);
    let actual = run(env!("CARGO_BIN_EXE_jsh"), jsh_dir.path(), script);
    assert_eq!(
        actual.status.code(),
        expected.status.code(),
        "{script}\njsh stderr: {}",
        String::from_utf8_lossy(&actual.stderr)
    );
    assert_eq!(
        actual.stdout,
        expected.stdout,
        "{script}\njsh stderr: {}",
        String::from_utf8_lossy(&actual.stderr)
    );
    for file in files {
        assert_eq!(
            std::fs::read(jsh_dir.path().join(file)).ok(),
            std::fs::read(bash_dir.path().join(file)).ok(),
            "{script}: file {file}"
        );
    }
}

#[test]
fn failed_redirect_prevents_builtin_function_group_and_external_execution() {
    for command in [
        "printf EXECUTED",
        "f",
        "{ printf EXECUTED; }",
        "(printf EXECUTED)",
        "/bin/sh -c 'printf EXECUTED'",
        "command /bin/sh -c 'printf EXECUTED'",
    ] {
        let script = format!(
            "f() {{ printf EXECUTED; }}; {command} > absent/output; printf '|%s\\n' \"$?\""
        );
        compare(&script, &[]);
    }
}

#[test]
fn redirects_without_commands_create_files_and_assignments_keep_error_status() {
    compare("> empty; printf old > truncated; > truncated; value=kept > absent/output; printf '<%s>:%s\\n' \"$value\" \"$?\"", &["empty", "truncated"]);
    compare("a=$(false) > empty; printf '%s\\n' $?", &["empty"]);
    compare("unset empty; $empty > created", &["created"]);
}

#[test]
fn failed_redirect_obeys_errexit_and_conditional_recovery() {
    compare(
        "set -e; printf NEVER > absent/output; printf ALSO_NEVER",
        &[],
    );
    compare(
        "set -e; printf NEVER > absent/output || printf recovered; printf after",
        &[],
    );
    compare(
        "trap 'printf ERR' ERR; printf NEVER > absent/output; printf after",
        &[],
    );
}

#[test]
fn redirects_apply_left_to_right_and_restore_after_partial_failure() {
    compare("printf payload > first > second; printf visible; printf NEVER > failed_first > absent/output; printf '|%s\\n' $?", &["first", "second", "failed_first"]);
    compare("/bin/sh -c 'printf out; printf err >&2' > first 2>&1; /bin/sh -c 'printf out; printf err >&2' 2>&1 > second; printf visible", &["first", "second"]);
    compare(
        "printf NEVER 2> errors > absent/output; printf visible",
        &[],
    );
}

#[test]
fn temporary_descriptors_return_to_their_originally_closed_state() {
    compare(": 3> out; printf NEVER >&3; printf '|%s\\n' $?", &["out"]);
    compare(
        "f() { printf function; }; f 3> out; printf NEVER >&3; printf '|%s\\n' $?",
        &["out"],
    );
    compare(
        "{ printf group; } 3> out; printf NEVER >&3; printf '|%s\\n' $?",
        &["out"],
    );
}

#[test]
fn backup_descriptors_cannot_become_user_duplication_sources() {
    // Before this fix stdout was saved using dup(), which could make a closed
    // user descriptor look open or be overwritten by a later redirection.
    for fd in [3, 10, 11, 12] {
        let dir = tempfile::tempdir().unwrap();
        let output = run(
            env!("CARGO_BIN_EXE_jsh"),
            dir.path(),
            &format!("printf NEVER > out 1>&{fd}; printf '|%s\\n' $?"),
        );
        assert_eq!(output.stdout, b"|1\n", "{:?}", output);
        assert_eq!(std::fs::read(dir.path().join("out")).unwrap(), b"");
    }
    compare(
        "printf payload > out 10>&1 11>&10; printf visible",
        &["out"],
    );
    let dir = tempfile::tempdir().unwrap();
    let output = run(
        env!("CARGO_BIN_EXE_jsh"),
        dir.path(),
        "fd=10; printf NEVER > out 1>&$fd; printf '|%s\\n' $?",
    );
    assert_eq!(output.stdout, b"|1\n");
}

#[test]
fn noclobber_preserves_existing_files_and_explicit_override_works() {
    compare("printf original > out; set -C; printf NEVER > out; printf '%s\\n' $?; cat out; printf replacement >| out; cat out; printf tail >> out; cat out", &["out"]);
    compare("printf original > out; set -C; /bin/sh -c 'printf NEVER' > out; printf '%s\\n' $?; cat out; /bin/sh -c 'printf replaced' 1>| out; cat out", &["out"]);
    compare("printf original > out; set -C; printf NEVER &> out; printf '%s\\n' $?; printf appended &>> out; cat out", &["out"]);
}

#[test]
fn noclobber_handles_symlinks_and_nonregular_outputs() {
    compare("printf original > out; ln -s out link; set -C; printf NEVER > link; printf '%s\\n' $?; cat out; printf ok > /dev/null; printf '%s\\n' $?; printf new > created", &["out", "created"]);
    compare(
        "ln -s missing link; set -C; printf NEVER > link; printf '%s\\n' $?",
        &["missing"],
    );
}

#[test]
fn redirection_expansion_requires_exactly_one_filename() {
    compare(
        "file='two words'; printf NEVER > $file; printf '%s\\n' $?; printf ok > \"$file\"",
        &["two words"],
    );
    compare(
        ": > a.txt; : > b.txt; printf NEVER > *.txt; printf '%s\\n' $?",
        &["a.txt", "b.txt"],
    );
    compare("unset file; printf NEVER > $file; printf '%s\\n' $?", &[]);
    let dir = tempfile::tempdir().unwrap();
    let output = run(
        env!("CARGO_BIN_EXE_jsh"),
        dir.path(),
        "set -u; printf NEVER > $missing; printf ALSO_NEVER",
    );
    // Bash's script shell exits with 127 on an unset variable.
    assert_eq!(output.status.code(), Some(127));
    assert!(output.stdout.is_empty());
}

#[test]
fn invalid_descriptor_duplication_fails_and_closed_descriptors_are_idempotent() {
    compare("printf NEVER 1>&99; printf '%s\\n' $?; : 99>&-; printf '%s\\n' $?; printf NEVER 2>&bad; printf '%s\\n' $?", &[]);
}

#[test]
fn exec_without_program_keeps_redirects() {
    compare("exec 3> out; printf first >&3; printf second >&3; exec 3>&-; printf NEVER >&3; printf '|%s\\n' $?; cat out", &["out"]);
    compare("exec > out; printf first; printf second", &["out"]);
}

#[test]
fn reopened_standard_fds_survive_exec_and_here_strings() {
    compare("/bin/sh -c 'printf payload' 1>&- > out; cat out", &["out"]);
    compare(
        "cat 0<&- <<< 'hello world'; read line 0<&- <<< 'another line'; printf '<%s>\\n' \"$line\"",
        &[],
    );
    compare(
        "cat <<< '$literal'; value='two words'; cat <<< \"$value\"",
        &[],
    );
}

#[test]
fn large_here_documents_are_complete_and_private_file_failures_are_errors() {
    let dir = tempfile::tempdir().unwrap();
    let body = "x".repeat(90_000);
    let output = run(
        env!("CARGO_BIN_EXE_jsh"),
        dir.path(),
        &format!("wc -c <<'EOF'\n{body}\nEOF\n"),
    );
    assert!(output.status.success(), "{:?}", output);
    assert_eq!(String::from_utf8_lossy(&output.stdout).trim(), "90001");
    let failed = Command::new(env!("CARGO_BIN_EXE_jsh"))
        .args(["-c", "printf NEVER <<< hello"])
        .env("TMPDIR", dir.path().join("absent"))
        .output()
        .unwrap();
    assert_eq!(failed.status.code(), Some(1));
    assert!(failed.stdout.is_empty());
    assert!(!failed.stderr.is_empty());
}

#[test]
fn typed_functions_and_bindings_honor_redirection_failures() {
    let dir = tempfile::tempdir().unwrap();
    let output = run(env!("CARGO_BIN_EXE_jsh"), dir.path(), "def add a:int b:int {|a,b| $a + $b}; add 2 3 > value; printf before; add 4 5 > absent/output; printf '|%s\\n' $?; let ignored = 42 > absent/output; printf '|%s\\n' $?");
    assert!(output.status.success(), "{:?}", output);
    assert_eq!(String::from_utf8_lossy(&output.stdout), "before|1\n|1\n");
    assert_eq!(
        std::fs::read_to_string(dir.path().join("value"))
            .unwrap()
            .trim(),
        "5"
    );
}

#[test]
fn nested_redirects_cannot_close_or_replace_an_ancestors_backup() {
    // Some Bash versions expose or close their own fd-10 backup in these
    // cases, so assert the intended descriptor isolation directly.
    for script in [
        "f() { exec 10>&-; }; f > out; printf after",
        "f() { exec 10>inner; }; f >out; printf after; printf inner >&10; exec 10>&-",
        "g() { exec 10>&-; exec 11>&-; exec 12>&-; }; f() { g > inner; printf outer; }; f > out; printf after",
    ] {
        let dir = tempfile::tempdir().unwrap();
        let output = run(env!("CARGO_BIN_EXE_jsh"), dir.path(), script);
        assert!(output.status.success(), "{script}: {output:?}");
        assert_eq!(output.stdout, b"after", "{script}: {output:?}");
        assert!(output.stderr.is_empty(), "{script}: {output:?}");
        if script.starts_with("g()") {
            assert_eq!(std::fs::read(dir.path().join("out")).unwrap(), b"outer");
        } else {
            assert_eq!(std::fs::read(dir.path().join("out")).unwrap(), b"");
        }
        if script.contains("printf inner") {
            assert_eq!(std::fs::read(dir.path().join("inner")).unwrap(), b"inner");
        }
    }
}

#[test]
fn exec_replaces_the_shell_and_preserves_argv_and_status() {
    compare(
        "exec /bin/sh -c 'printf \"<%s>\" \"$1\"; exit 7' worker 'a b'; printf NEVER",
        &[],
    );
    compare("exec -a custom /bin/sh -c 'printf \"%s\" \"$0\"'", &[]);
    compare("exec -c /usr/bin/env", &[]);
    compare("exec jsh_no_such_exec_program_861f725a; printf NEVER", &[]);
    compare(
        "shopt -s execfail; exec jsh_no_such_exec_program_861f725a; printf '|%s' $?",
        &[],
    );
    let dir = tempfile::tempdir().unwrap();
    let output = run(
        env!("CARGO_BIN_EXE_jsh"),
        dir.path(),
        "printf '%s:' $$; exec /bin/sh -c 'printf %s $$'",
    );
    let text = String::from_utf8(output.stdout).unwrap();
    let (before, after) = text.split_once(':').unwrap();
    assert_eq!(before, after, "exec must retain its process ID");
}

#[test]
fn here_docs_preserve_blank_lines_carriage_returns_and_trailing_newlines() {
    compare("cat <<'EOF'\n\nfirst\r\n\nlast\nEOF\n", &[]);
    compare("cat <<- 'EOF'\n\tfirst\n\t\n\tlast\n\tEOF\n", &[]);
}

#[test]
fn exec_resets_child_signal_dispositions_and_restores_them_on_failure() {
    compare("exec /bin/sh -c 'kill -PIPE $$; printf NEVER'", &[]);
    let dir = tempfile::tempdir().unwrap();
    let output = run(
        env!("CARGO_BIN_EXE_jsh"),
        dir.path(),
        "shopt -s execfail; exec jsh_no_such_exec_program_861f725a; kill -TERM $$; printf NEVER",
    );
    // jsh translates a caught TERM into an orderly shutdown with status 143.
    assert_eq!(output.status.code(), Some(143));
    assert!(output.stdout.is_empty());
}

#[cfg(target_os = "linux")]
#[test]
fn buffered_scalar_output_errors_return_failure_before_restoring_stdout() {
    let dir = tempfile::tempdir().unwrap();
    let output = run(
        env!("CARGO_BIN_EXE_jsh"),
        dir.path(),
        "def answer {| | 42}; answer > /dev/full; printf 'status=%s' $?",
    );
    assert!(output.status.success(), "{output:?}");
    assert_eq!(output.stdout, b"status=1");
    assert!(!output.stderr.is_empty());
}

#[test]
fn command_exec_preserves_its_special_scope_and_prefix_environment() {
    compare("command exec > out; printf visible", &["out"]);
    compare("command -p -- command exec > out; printf visible", &["out"]);
    compare("builtin exec > out; printf visible", &["out"]);
    compare("command -v exec > out; printf visible", &["out"]);
    compare(
        "f() { JSH_EXEC_TEST=expected exec /bin/sh -c 'printf \"<%s>\" \"$JSH_EXEC_TEST\"'; }; f",
        &[],
    );
    compare("f() { JSH_EXEC_TEST=expected command exec /bin/sh -c 'printf \"<%s>\" \"$JSH_EXEC_TEST\"'; }; f", &[]);
    compare("f() { shopt -s execfail; JSH_EXEC_TEST=temporary exec no_jsh_exec_program_861f725a; /bin/sh -c 'printf \"<%s>\" \"${JSH_EXEC_TEST-unset}\"'; }; f", &[]);
}
