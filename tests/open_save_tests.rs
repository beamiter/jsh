use std::io::Write;
/// Phase 6a — `open` and `save` (file I/O bridges to converters).
use std::process::{Command, Stdio};

fn jsh_bin() -> String {
    env!("CARGO_BIN_EXE_jsh").to_string()
}

fn run_in(dir: &std::path::Path, script: &str) -> (String, String, i32) {
    let child = Command::new(jsh_bin())
        .current_dir(dir)
        .arg("-c")
        .arg(script)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .expect("spawn");
    let out = child.wait_with_output().expect("wait");
    (
        String::from_utf8_lossy(&out.stdout).into_owned(),
        String::from_utf8_lossy(&out.stderr).into_owned(),
        out.status.code().unwrap_or(-1),
    )
}

fn run_with_stdin(dir: &std::path::Path, script: &str, stdin: &str) -> (String, String, i32) {
    let mut child = Command::new(jsh_bin())
        .current_dir(dir)
        .arg("-c")
        .arg(script)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .expect("spawn");
    child
        .stdin
        .as_mut()
        .unwrap()
        .write_all(stdin.as_bytes())
        .unwrap();
    let out = child.wait_with_output().expect("wait");
    (
        String::from_utf8_lossy(&out.stdout).into_owned(),
        String::from_utf8_lossy(&out.stderr).into_owned(),
        out.status.code().unwrap_or(-1),
    )
}

#[test]
fn open_json_then_filter() {
    let dir = tempfile::tempdir().unwrap();
    std::fs::write(dir.path().join("t.json"), r#"[{"a":1},{"a":3}]"#).unwrap();
    let (out, err, code) = run_in(
        dir.path(),
        r#"open t.json | where {|r| [ $r.a -gt 1 ]} | to-json"#,
    );
    assert_eq!(code, 0, "stderr: {}", err);
    let p: serde_json::Value = serde_json::from_str(out.trim()).unwrap();
    assert_eq!(p, serde_json::json!([{"a":3}]));
}

#[test]
fn open_yaml_dispatches_to_from_yaml() {
    let dir = tempfile::tempdir().unwrap();
    std::fs::write(dir.path().join("t.yaml"), "k: v\nn: 7\n").unwrap();
    let (out, err, code) = run_in(dir.path(), "open t.yaml | to-json");
    assert_eq!(code, 0, "stderr: {}", err);
    let p: serde_json::Value = serde_json::from_str(out.trim()).unwrap();
    let obj = if p.is_array() { p[0].clone() } else { p };
    assert_eq!(obj, serde_json::json!({"k":"v","n":7}));
}

#[test]
fn open_text_returns_string() {
    let dir = tempfile::tempdir().unwrap();
    std::fs::write(dir.path().join("t.txt"), "hello\n").unwrap();
    let (out, err, code) = run_in(dir.path(), "open t.txt");
    assert_eq!(code, 0, "stderr: {}", err);
    assert_eq!(out.trim(), "hello");
}

#[test]
fn open_missing_file_fails() {
    let dir = tempfile::tempdir().unwrap();
    let (_out, _err, code) = run_in(dir.path(), "open nope.json");
    assert_ne!(code, 0);
}

#[test]
fn save_json_writes_file() {
    let dir = tempfile::tempdir().unwrap();
    std::fs::write(dir.path().join("src.json"), r#"[{"a":1},{"a":2}]"#).unwrap();
    let (_out, err, code) = run_in(dir.path(), "open src.json | save dst.yaml");
    assert_eq!(code, 0, "stderr: {}", err);
    let content = std::fs::read_to_string(dir.path().join("dst.yaml")).unwrap();
    assert!(content.contains("a: 1"));
    assert!(content.contains("a: 2"));
}

#[test]
fn save_passthrough_text_bytes() {
    let dir = tempfile::tempdir().unwrap();
    let (_out, err, code) = run_with_stdin(dir.path(), "save out.txt", "abc\n");
    assert_eq!(code, 0, "stderr: {}", err);
    assert_eq!(
        std::fs::read_to_string(dir.path().join("out.txt")).unwrap(),
        "abc\n"
    );
}

#[test]
fn save_append_concatenates() {
    let dir = tempfile::tempdir().unwrap();
    let _ = run_with_stdin(dir.path(), "save out.txt", "first\n");
    let (_out, err, code) = run_with_stdin(dir.path(), "save --append out.txt", "second\n");
    assert_eq!(code, 0, "stderr: {}", err);
    let s = std::fs::read_to_string(dir.path().join("out.txt")).unwrap();
    assert_eq!(s, "first\nsecond\n");
}

#[test]
fn round_trip_json_to_yaml_to_json() {
    let dir = tempfile::tempdir().unwrap();
    std::fs::write(dir.path().join("a.json"), r#"[{"k":"v"}]"#).unwrap();
    let _ = run_in(dir.path(), "open a.json | save b.yaml");
    let (out, err, code) = run_in(dir.path(), "open b.yaml | to-json");
    assert_eq!(code, 0, "stderr: {}", err);
    let p: serde_json::Value = serde_json::from_str(out.trim()).unwrap();
    assert_eq!(p, serde_json::json!([{"k":"v"}]));
}
