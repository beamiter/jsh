/// Phase 5d — format converters (`from-yaml`/`to-yaml`, `from-toml`/`to-toml`,
/// `from-xml`/`to-xml`) and structured `ls`/`ps`.
use std::io::Write;
use std::process::{Command, Stdio};

fn jsh_bin() -> String {
    env!("CARGO_BIN_EXE_jsh").to_string()
}

fn run(script: &str, stdin: &str) -> (String, String, i32) {
    let mut child = Command::new(jsh_bin())
        .arg("-c")
        .arg(script)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .expect("spawn jsh");
    if !stdin.is_empty() {
        child
            .stdin
            .as_mut()
            .unwrap()
            .write_all(stdin.as_bytes())
            .unwrap();
    }
    let out = child.wait_with_output().expect("wait");
    (
        String::from_utf8_lossy(&out.stdout).to_string(),
        String::from_utf8_lossy(&out.stderr).to_string(),
        out.status.code().unwrap_or(-1),
    )
}

// ---------------------------------------------------------------------------
// YAML
// ---------------------------------------------------------------------------

#[test]
fn yaml_record_to_json() {
    // from-yaml wraps a single record in a one-element list (same shape as
    // from-json + Values pipeline), so to-json emits an array.
    let stdin = "name: alice\nage: 30\n";
    let (out, err, code) = run("from-yaml | to-json", stdin);
    assert_eq!(code, 0, "stderr: {}", err);
    let parsed: serde_json::Value = serde_json::from_str(out.trim()).unwrap();
    let obj = if parsed.is_array() {
        parsed[0].clone()
    } else {
        parsed
    };
    assert_eq!(obj, serde_json::json!({"name":"alice","age":30}));
}

#[test]
fn yaml_round_trip_preserves_record() {
    let stdin = r#"{"k":"v","n":42}"#;
    let (out, err, code) = run("from-json | to-yaml | from-yaml | to-json", stdin);
    assert_eq!(code, 0, "stderr: {}", err);
    let parsed: serde_json::Value = serde_json::from_str(out.trim()).unwrap();
    let obj = if parsed.is_array() {
        parsed[0].clone()
    } else {
        parsed
    };
    assert_eq!(obj, serde_json::json!({"k":"v","n":42}));
}

// ---------------------------------------------------------------------------
// TOML
// ---------------------------------------------------------------------------

#[test]
fn toml_to_json() {
    let stdin = "name = \"alice\"\nage = 30\n";
    let (out, err, code) = run("from-toml | to-json", stdin);
    assert_eq!(code, 0, "stderr: {}", err);
    let parsed: serde_json::Value = serde_json::from_str(out.trim()).unwrap();
    // from-toml wraps the top-level table in a single-record list
    let obj = if parsed.is_array() {
        parsed[0].clone()
    } else {
        parsed
    };
    assert_eq!(obj["name"], serde_json::json!("alice"));
    assert_eq!(obj["age"], serde_json::json!(30));
}

// ---------------------------------------------------------------------------
// XML
// ---------------------------------------------------------------------------

#[test]
fn xml_parses_nested_elements() {
    let stdin = r#"<root><a>1</a><b foo="bar">x</b></root>"#;
    let (out, err, code) = run("from-xml | to-json", stdin);
    assert_eq!(code, 0, "stderr: {}", err);
    let parsed: serde_json::Value = serde_json::from_str(out.trim()).unwrap();
    let root = &parsed[0];
    assert_eq!(root["tag"], serde_json::json!("root"));
    let kids = root["children"].as_array().expect("children");
    assert_eq!(kids[0]["tag"], serde_json::json!("a"));
    assert_eq!(kids[0]["text"], serde_json::json!("1"));
    assert_eq!(kids[1]["attrs"]["foo"], serde_json::json!("bar"));
}

#[test]
fn xml_self_closing_element() {
    let stdin = r#"<root><c/></root>"#;
    let (out, err, code) = run("from-xml | to-json", stdin);
    assert_eq!(code, 0, "stderr: {}", err);
    let parsed: serde_json::Value = serde_json::from_str(out.trim()).unwrap();
    let kids = parsed[0]["children"].as_array().expect("children");
    assert_eq!(kids[0]["tag"], serde_json::json!("c"));
}

// ---------------------------------------------------------------------------
// Structured `ls`
// ---------------------------------------------------------------------------

#[test]
fn ls_produces_records_with_size_and_type() {
    // ls of the project root: filter to Cargo.toml.
    let (out, err, code) = run(r#"ls | where {|r| [ $r.name = Cargo.toml ]} | to-json"#, "");
    assert_eq!(code, 0, "stderr: {}", err);
    let parsed: serde_json::Value = serde_json::from_str(out.trim()).unwrap();
    let arr = parsed.as_array().expect("list");
    assert_eq!(arr.len(), 1);
    assert_eq!(arr[0]["name"], serde_json::json!("Cargo.toml"));
    assert_eq!(arr[0]["type"], serde_json::json!("file"));
    assert!(arr[0]["size"].as_i64().unwrap_or(0) > 0);
}

#[test]
fn ls_dispatch_bare_runs_text_path() {
    // Single-command pipeline should bypass value-aware and produce plain text.
    let (out, err, code) = run("ls", "");
    assert_eq!(code, 0, "stderr: {}", err);
    // text-path `ls` doesn't emit JSON
    assert!(!out.trim_start().starts_with('['));
}

// ---------------------------------------------------------------------------
// Structured `ps`
// ---------------------------------------------------------------------------

#[test]
fn ps_records_have_pid_and_name() {
    let (out, err, code) = run("ps | first 1 | to-json", "");
    assert_eq!(code, 0, "stderr: {}", err);
    let parsed: serde_json::Value = serde_json::from_str(out.trim()).unwrap();
    let arr = parsed.as_array().expect("list");
    assert_eq!(arr.len(), 1);
    assert!(arr[0]["pid"].as_i64().is_some());
    assert!(arr[0]["name"].is_string());
}
