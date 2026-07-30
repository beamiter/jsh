/// Two classes of real-world bash incompatibility, both verified against
/// /bin/bash on the same input:
///   * a malformed `${...}` body (`${a[}`, `${}`) and a brace range at the
///     integer limits aborted the shell with a Rust panic instead of reporting
///     bash's diagnostic — a typo at the prompt killed the process
///   * `${BASH_SOURCE[0]}` was set only while *sourcing*, so an executed script
///     saw it empty and the near-universal
///     `SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"` silently
///     resolved to the caller's cwd
use std::path::Path;
use std::process::{Command, Output, Stdio};

fn jsh_bin() -> String {
    env!("CARGO_BIN_EXE_jsh").to_string()
}

fn text(bytes: &[u8]) -> String {
    String::from_utf8_lossy(bytes).into_owned()
}

fn run(script: &str) -> (String, String, i32) {
    let out = Command::new(jsh_bin())
        .arg("-c")
        .arg(script)
        .stdin(Stdio::null())
        .output()
        .expect("spawn jsh");
    (
        text(&out.stdout),
        text(&out.stderr),
        out.status.code().unwrap_or(-1),
    )
}

/// Run a script file, optionally from another working directory and with `dir`
/// prepended to `$PATH`, the way a user invokes `jsh path/to/script.sh`.
fn run_script(arg: &str, cwd: &Path, path_dir: Option<&Path>) -> Output {
    let mut cmd = Command::new(jsh_bin());
    cmd.arg(arg).current_dir(cwd).stdin(Stdio::null());
    if let Some(dir) = path_dir {
        // Keep the inherited PATH: the SCRIPT_DIR idiom shells out to `dirname`.
        let inherited = std::env::var("PATH").unwrap_or_default();
        cmd.env("PATH", format!("{}:{}", dir.display(), inherited));
    }
    cmd.output().expect("run script")
}

// ---------------------------------------------------------------------------
// Malformed ${...} bodies
// ---------------------------------------------------------------------------

/// `${a[}` parses as the parameter name `a[`, where slicing out the subscript is
/// the reversed byte range 2..1. That panicked the process (exit 101); bash
/// reports "bad substitution" and exits 1.
#[test]
fn a_malformed_parameter_body_reports_bad_substitution_instead_of_panicking() {
    for body in ["${a[}", "${a[0}", "${a[]}", "${}"] {
        let (out, err, code) = run(&format!("echo {}", body));
        assert_eq!(code, 1, "{body}: expected bash's status 1, stderr: {err}");
        assert!(
            err.contains("bad substitution"),
            "{body}: expected bash's diagnostic, got: {err}"
        );
        assert!(
            err.contains(body),
            "{body}: diagnostic should quote the body: {err}"
        );
        assert!(out.is_empty(), "{body}: unexpected stdout: {out}");
        assert!(!err.contains("panicked"), "{body}: shell panicked: {err}");
    }
}

/// A bad substitution aborts the rest of the program, exactly as in bash.
#[test]
fn a_bad_substitution_stops_the_program() {
    let (out, _, code) = run("echo ${a[}; echo after");
    assert_eq!(code, 1);
    assert!(!out.contains("after"), "execution continued past the error");
}

/// The forms that only *look* malformed must keep working: these are all valid
/// in bash and none of them is an error.
#[test]
fn well_formed_bodies_that_resemble_a_bad_subscript_still_expand() {
    for (script, expected) in [
        (r#"echo "[${#}]""#, "[0]\n"),
        (r#"echo "[${!}]""#, "[]\n"),
        (r#"echo "[${a[@]}]""#, "[]\n"),
        (r#"echo "[${a[*]}]""#, "[]\n"),
        (r#"v=xa-by; echo "[${v##*a-b*}]""#, "[]\n"),
        (r#"v=abc; echo "[${v#[a-z]}]""#, "[bc]\n"),
        (r#"v=abc; echo "[${v%[a-z]}]""#, "[ab]\n"),
        (r#"a=(x y); echo "[${a[1]}]""#, "[y]\n"),
    ] {
        let (out, err, code) = run(script);
        assert_eq!(code, 0, "{script}: stderr: {err}");
        assert!(err.is_empty(), "{script}: unexpected stderr: {err}");
        assert_eq!(out, expected, "{script}");
    }
}

/// A brace range that walks off the end of i64/i32 aborted the shell with
/// "attempt to add with overflow"; an i64::MIN step aborted it inside `abs()`.
#[test]
fn brace_ranges_at_the_integer_limits_do_not_abort_the_shell() {
    for (script, expected) in [
        (
            "echo {9223372036854775806..9223372036854775807}",
            "9223372036854775806 9223372036854775807\n",
        ),
        ("echo {1..3..9223372036854775807}", "1\n"),
        ("echo {a..z..-2147483648}", "a\n"),
        ("echo {1..5}", "1 2 3 4 5\n"),
        ("echo {01..05}", "01 02 03 04 05\n"),
    ] {
        let (out, err, code) = run(script);
        assert_eq!(code, 0, "{script}: stderr: {err}");
        assert!(!err.contains("panicked"), "{script}: shell panicked: {err}");
        assert_eq!(out, expected, "{script}");
    }
}

/// Bash treats a plain scalar as a one-element array, which is why defensive
/// code like `${BASH_SOURCE[0]}` and `${#BASH_SOURCE[@]}` works before anything
/// has declared an array. jsh expanded every one of these to nothing.
#[test]
fn a_scalar_answers_array_subscripts_like_bash() {
    for (script, expected) in [
        (r#"x=abc; echo "[${x[0]}]""#, "[abc]\n"),
        (r#"x=abc; echo "[${x[1]}]""#, "[]\n"),
        (r#"x=abc; echo "[${x[@]}]""#, "[abc]\n"),
        (r#"x=abc; echo "[${#x[@]}]""#, "[1]\n"),
        (r#"x=abc; echo "[${#x[0]}]""#, "[3]\n"),
        (r#"x=abc; echo "[${!x[@]}]""#, "[0]\n"),
        (r#"x=""; echo "[${#x[@]}]""#, "[1]\n"),
        (r#"echo "[${#unset_name[@]}]""#, "[0]\n"),
        (r#"echo "[${!unset_name[@]}]""#, "[]\n"),
        (r#"echo "[${unset_name[0]}]""#, "[]\n"),
        (
            r#"x=abc; for e in "${x[@]}"; do echo "e=$e"; done"#,
            "e=abc\n",
        ),
        // Real arrays keep their existing behaviour, empty ones included.
        (
            r#"a=(1 2 3); echo "[${a[@]}][${#a[@]}][${!a[@]}]""#,
            "[1 2 3][3][0 1 2]\n",
        ),
        (r#"a=(); set -- "${a[@]}"; echo "[$#]""#, "[0]\n"),
        (
            r#"a=(one two); set -- "${a[@]}"; echo "[$#][$1][$2]""#,
            "[2][one][two]\n",
        ),
    ] {
        let (out, err, code) = run(script);
        assert_eq!(code, 0, "{script}: stderr: {err}");
        assert!(err.is_empty(), "{script}: unexpected stderr: {err}");
        assert_eq!(out, expected, "{script}");
    }
}

// ---------------------------------------------------------------------------
// BASH_SOURCE for an executed script
// ---------------------------------------------------------------------------

const REPORT: &str = concat!(
    "printf '%s|%s|%s\\n' \"$0\" \"${BASH_SOURCE[0]}\" \"${#BASH_SOURCE[@]}\"\n",
    "SCRIPT_DIR=\"$(cd \"$(dirname \"${BASH_SOURCE[0]}\")\" && pwd)\"\n",
    "echo \"$SCRIPT_DIR\"\n",
);

/// The failure users actually hit: the script lives somewhere other than the
/// caller's cwd, so an empty `${BASH_SOURCE[0]}` makes `dirname` answer `.` and
/// SCRIPT_DIR silently becomes the *caller's* directory.
#[test]
fn an_executed_script_locates_its_own_directory_from_another_cwd() {
    let root = tempfile::tempdir().expect("tempdir");
    let sub = root.path().join("sub");
    std::fs::create_dir(&sub).expect("mkdir sub");
    std::fs::write(sub.join("setup.sh"), REPORT).expect("write script");
    let sub_real = sub.canonicalize().expect("canonicalize");

    // Relative to the caller's cwd.
    let out = run_script("sub/setup.sh", root.path(), None);
    assert!(out.status.success(), "{}", text(&out.stderr));
    let stdout = text(&out.stdout);
    let lines: Vec<&str> = stdout.lines().collect();
    assert_eq!(lines[0], "sub/setup.sh|sub/setup.sh|1");
    assert_eq!(lines[1], sub_real.display().to_string());

    // Absolute path.
    let abs = sub.join("setup.sh");
    let out = run_script(abs.to_str().unwrap(), root.path(), None);
    assert!(out.status.success(), "{}", text(&out.stderr));
    let stdout = text(&out.stdout);
    let lines: Vec<&str> = stdout.lines().collect();
    assert_eq!(lines[0], format!("{}|{}|1", abs.display(), abs.display()));
    assert_eq!(lines[1], sub_real.display().to_string());

    // `./script.sh` from the script's own directory.
    let out = run_script("./setup.sh", &sub, None);
    assert!(out.status.success(), "{}", text(&out.stderr));
    let stdout = text(&out.stdout);
    let lines: Vec<&str> = stdout.lines().collect();
    assert_eq!(lines[0], "./setup.sh|./setup.sh|1");
    assert_eq!(lines[1], sub_real.display().to_string());
}

/// Bash searches `$PATH` for a bare script name; `$0` stays the name as typed
/// while `${BASH_SOURCE[0]}` names the file that was actually opened, so
/// SCRIPT_DIR still finds the script's own directory.
#[test]
fn a_script_found_on_path_reports_the_file_it_opened() {
    let root = tempfile::tempdir().expect("tempdir");
    let bin = root.path().join("bin");
    let elsewhere = root.path().join("elsewhere");
    std::fs::create_dir(&bin).expect("mkdir bin");
    std::fs::create_dir(&elsewhere).expect("mkdir elsewhere");
    std::fs::write(bin.join("onpath.sh"), REPORT).expect("write script");

    let out = run_script("onpath.sh", &elsewhere, Some(&bin));
    assert!(out.status.success(), "{}", text(&out.stderr));
    let stdout = text(&out.stdout);
    let lines: Vec<&str> = stdout.lines().collect();
    assert_eq!(lines[0], format!("onpath.sh|{}/onpath.sh|1", bin.display()));
    assert_eq!(
        lines[1],
        bin.canonicalize()
            .expect("canonicalize")
            .display()
            .to_string()
    );
}

/// `source` pushes a BASH_SOURCE frame in front of the caller's. With no frame
/// for the executed script there was nothing to push in front of, so a sourced
/// helper could not see which file had sourced it.
#[test]
fn sourcing_from_an_executed_script_stacks_a_frame_on_top_of_it() {
    let root = tempfile::tempdir().expect("tempdir");
    std::fs::write(
        root.path().join("lib.sh"),
        "printf 'lib|%s|%s|%s\\n' \"${BASH_SOURCE[0]}\" \"${BASH_SOURCE[1]}\" \"${#BASH_SOURCE[@]}\"\n",
    )
    .expect("write lib");
    std::fs::write(
        root.path().join("outer.sh"),
        concat!(
            "printf 'outer|%s|%s\\n' \"${BASH_SOURCE[0]}\" \"${#BASH_SOURCE[@]}\"\n",
            "source \"$(dirname \"${BASH_SOURCE[0]}\")/lib.sh\"\n",
            "printf 'back|%s|%s\\n' \"${BASH_SOURCE[0]}\" \"${#BASH_SOURCE[@]}\"\n",
        ),
    )
    .expect("write outer");

    let out = run_script("./outer.sh", root.path(), None);
    assert!(out.status.success(), "{}", text(&out.stderr));
    assert_eq!(
        text(&out.stdout),
        "outer|./outer.sh|1\nlib|./lib.sh|./outer.sh|2\nback|./outer.sh|1\n"
    );
}
