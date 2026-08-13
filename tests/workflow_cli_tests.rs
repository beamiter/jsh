use std::process::Command;

fn run(script: &str) -> (String, String, i32) {
    let home = tempfile::tempdir().expect("isolated workflow home");
    let output = Command::new(env!("CARGO_BIN_EXE_jsh"))
        .args(["--norc", "-c", script])
        .env("HOME", home.path())
        .output()
        .expect("run jsh");
    (
        String::from_utf8_lossy(&output.stdout).into_owned(),
        String::from_utf8_lossy(&output.stderr).into_owned(),
        output.status.code().unwrap_or(-1),
    )
}

#[test]
fn workflow_builtin_lists_and_shows_the_shared_registry() {
    let (list, error, status) = run("workflow list");
    assert_eq!(status, 0, "{error}");
    assert!(list.contains("http-serve"), "{list}");

    let (show, error, status) = run("wf show http-serve");
    assert_eq!(status, 0, "{error}");
    assert!(show.contains("python3 -m http.server {{port}}"), "{show}");
    assert!(show.contains("port"), "{show}");
}

#[test]
fn workflow_render_fills_defaults_and_overrides_without_executing() {
    let (output, error, status) = run("workflow render http-serve port=9090");
    assert_eq!(status, 0, "{error}");
    assert_eq!(output, "python3 -m http.server 9090\n");
}

#[test]
fn workflow_render_refuses_missing_and_unknown_parameters() {
    let (_, missing, status) = run("workflow render tar-compress output=archive");
    assert_eq!(status, 2);
    assert!(missing.contains("missing value for 'input'"), "{missing}");

    let (_, unknown, status) = run("workflow render http-serve typo=1");
    assert_eq!(status, 2);
    assert!(unknown.contains("no parameter 'typo'"), "{unknown}");

    let (_, duplicate, status) = run("workflow render http-serve port=1 port=2");
    assert_eq!(status, 2);
    assert!(duplicate.contains("duplicate value"), "{duplicate}");
}
