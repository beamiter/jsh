/// Phase 16 — module `use`, par-each, http, signature hints.
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

fn write_temp(name: &str, content: &str) -> String {
    let dir = std::env::temp_dir();
    let path = dir.join(name);
    std::fs::write(&path, content).expect("write temp file");
    path.to_string_lossy().to_string()
}

// ---------------------------------------------------------------------------
// 16a — use / module import
// ---------------------------------------------------------------------------

#[test]
fn use_imports_def_functions_from_file() {
    let p = write_temp(
        "jsh_phase16_basic.jsh",
        "def add a:int b:int {|a,b| $a + $b}\ndef mul a:int b:int {|a,b| $a * $b}\n",
    );
    let (out, _, code) = run(&format!("use {}; add 3 4", p), "");
    assert_eq!(code, 0);
    assert_eq!(out.trim(), "7");

    let (out, _, code) = run(&format!("use {}; mul 6 7", p), "");
    assert_eq!(code, 0);
    assert_eq!(out.trim(), "42");
}

#[test]
fn use_selective_keeps_only_named() {
    let p = write_temp(
        "jsh_phase16_select.jsh",
        "def add a:int b:int {|a,b| $a + $b}\ndef mul a:int b:int {|a,b| $a * $b}\n",
    );
    let (_, err, code) = run(&format!("use {} add; mul 1 2", p), "");
    assert_ne!(code, 0);
    assert!(
        err.contains("mul: command not found") || err.contains("not found"),
        "stderr was: {}",
        err
    );
}

#[test]
fn use_missing_file_errors_cleanly() {
    let (_, err, code) = run("use /tmp/does_not_exist_jsh_phase16.jsh", "");
    assert_ne!(code, 0);
    assert!(err.contains("use:"), "stderr was: {}", err);
}

// ---------------------------------------------------------------------------
// 16b — par-each
// ---------------------------------------------------------------------------

#[test]
fn par_each_preserves_input_order() {
    let (out, _, code) = run("range 1..10 | par-each {|x| $x * $x} | to-json", "");
    assert_eq!(code, 0);
    let compact: String = out.chars().filter(|c| !c.is_whitespace()).collect();
    assert_eq!(compact, "[1,4,9,16,25,36,49,64,81,100]");
}

#[test]
fn par_each_length_matches_input() {
    let (out, _, code) = run("range 1..200 | par-each {|x| $x + 1} | length", "");
    assert_eq!(code, 0);
    assert_eq!(out.trim(), "200");
}

#[test]
fn par_each_with_threads_flag() {
    let (out, _, code) = run("range 1..20 | par-each -t 2 {|x| $x * 10} | to-json", "");
    assert_eq!(code, 0);
    let compact: String = out.chars().filter(|c| !c.is_whitespace()).collect();
    assert_eq!(
        compact,
        "[10,20,30,40,50,60,70,80,90,100,110,120,130,140,150,160,170,180,190,200]"
    );
}

#[test]
fn par_each_on_empty_input_returns_empty() {
    let (out, _, code) = run("range 1..<1 | par-each {|x| $x * 2} | to-json", "");
    assert_eq!(code, 0);
    let compact: String = out.chars().filter(|c| !c.is_whitespace()).collect();
    assert_eq!(compact, "[]");
}

// ---------------------------------------------------------------------------
// 16c — http client (uses a tiny in-process TCP listener; no external net)
// ---------------------------------------------------------------------------

#[cfg(feature = "ai")]
use std::net::TcpListener;

#[cfg(feature = "ai")]
fn spawn_stub_server(
    response_body: &'static str,
    content_type: &'static str,
) -> Option<(String, std::thread::JoinHandle<Option<String>>)> {
    let listener = match TcpListener::bind("127.0.0.1:0") {
        Ok(l) => l,
        Err(e) => {
            eprintln!("skip phase16 http test: bind unavailable ({})", e);
            return None;
        }
    };
    use std::io::Read as _;

    let addr = listener.local_addr().expect("addr");
    let url = format!("http://{}/", addr);
    let handle = std::thread::spawn(move || {
        let (mut sock, _) = listener.accept().ok()?;
        sock.set_read_timeout(Some(std::time::Duration::from_secs(2)))
            .ok()?;
        let mut request = Vec::new();
        let mut buf = [0u8; 4096];
        loop {
            let n = sock.read(&mut buf).ok()?;
            if n == 0 {
                break;
            }
            request.extend_from_slice(&buf[..n]);
            if request.len() > 64 * 1024 {
                return None;
            }
            let Some(header_end) = request.windows(4).position(|bytes| bytes == b"\r\n\r\n") else {
                continue;
            };
            let body_start = header_end + 4;
            let headers = String::from_utf8_lossy(&request[..header_end]);
            let content_length = headers.lines().find_map(|line| {
                let (name, value) = line.split_once(':')?;
                name.eq_ignore_ascii_case("content-length")
                    .then(|| value.trim().parse::<usize>().ok())
                    .flatten()
            });
            if request.len() >= body_start + content_length.unwrap_or(0) {
                break;
            }
        }
        let req = String::from_utf8_lossy(&request).into_owned();
        let body = response_body.as_bytes();
        let header = format!(
            "HTTP/1.1 200 OK\r\nContent-Type: {}\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
            content_type, body.len()
        );
        let _ = sock.write_all(header.as_bytes());
        let _ = sock.write_all(body);
        Some(req)
    });
    Some((url, handle))
}

#[cfg(feature = "ai")]
#[test]
fn http_get_parses_json_body() {
    let Some((url, _h)) = spawn_stub_server(r#"{"hello":"world","n":42}"#, "application/json")
    else {
        return;
    };
    let (out, _, code) = run(&format!("http get {} | get body | get hello", url), "");
    assert_eq!(code, 0);
    assert_eq!(out.trim(), "world");
}

#[cfg(feature = "ai")]
#[test]
fn http_get_returns_status_code() {
    let Some((url, _h)) = spawn_stub_server(r#"{"ok":true}"#, "application/json") else {
        return;
    };
    let (out, _, code) = run(&format!("http get {} | get status", url), "");
    assert_eq!(code, 0);
    assert_eq!(out.trim(), "200");
}

#[cfg(feature = "ai")]
#[test]
fn http_get_plain_text_body_kept_as_string() {
    let Some((url, _h)) = spawn_stub_server("plain hello", "text/plain") else {
        return;
    };
    let (out, _, code) = run(&format!("http get {} | get body", url), "");
    assert_eq!(code, 0);
    assert_eq!(out.trim(), "plain hello");
}

#[cfg(feature = "ai")]
#[test]
fn http_post_sends_body() {
    let Some((url, h)) = spawn_stub_server(r#"{"ok":1}"#, "application/json") else {
        return;
    };
    let (_, _, code) = run(&format!("http post {} -d \"abc=123\" --json", url), "");
    assert_eq!(code, 0);
    let req = h.join().expect("join").expect("request seen");
    assert!(req.starts_with("POST "), "request was: {}", req);
    assert!(req.contains("abc=123"), "request was: {}", req);
}

#[cfg(feature = "ai")]
#[test]
fn http_unknown_method_errors() {
    let (_, err, code) = run("http bogus https://example.com", "");
    assert_ne!(code, 0);
    assert!(err.contains("unknown method"), "stderr was: {}", err);
}

#[cfg(feature = "ai")]
#[test]
fn http_missing_url_errors() {
    let (_, err, code) = run("http get", "");
    assert_ne!(code, 0);
    assert!(
        err.contains("missing URL") || err.contains("missing"),
        "stderr was: {}",
        err
    );
}
