//! The model request runs in a child process so that cancelling it ends it.
//!
//! A blocking socket read cannot be interrupted in place, so the previous
//! worker-thread transport could only ever be *abandoned*: the foreground was
//! released promptly, but the request kept running — connected, billed, and
//! holding the single-flight slot — until the provider's own read timeout
//! expired. These tests pin the two properties that changed: the request really
//! is performed by a separate process, and killing that process ends the
//! request now rather than minutes later.

// Without the `ai` feature there is no transport to test: the flag, the child
// entry point and the agent builtin are all compiled out.
#![cfg(feature = "ai")]

use std::io::{Read, Write};
use std::net::TcpListener;
use std::process::{Command, Stdio};
use std::sync::mpsc;
use std::time::{Duration, Instant};

const FLAG: &str = "--jsh-internal-model-request";

fn envelope(url: &str) -> String {
    format!(
        r#"{{"v":1,"provider":"ollama","url":"{url}","headers":[["content-type","application/json"]],"body":"{{}}"}}"#
    )
}

/// A child in its own process group, fed one request envelope on stdin.
fn spawn_transport(envelope: &str) -> std::process::Child {
    let mut child = Command::new(env!("CARGO_BIN_EXE_jsh"))
        .arg(FLAG)
        .env("NO_PROXY", "127.0.0.1,localhost")
        .env_remove("HTTP_PROXY")
        .env_remove("HTTPS_PROXY")
        .env_remove("ALL_PROXY")
        .env_remove("http_proxy")
        .env_remove("https_proxy")
        .env_remove("all_proxy")
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .expect("spawn transport child");
    let mut stdin = child.stdin.take().expect("transport stdin");
    stdin
        .write_all(envelope.as_bytes())
        .expect("write envelope");
    drop(stdin);
    child
}

#[test]
fn the_transport_child_performs_the_request_and_frames_its_answer() {
    let listener = TcpListener::bind("127.0.0.1:0").expect("bind provider");
    let address = listener.local_addr().expect("provider address");
    let (reached_tx, reached_rx) = mpsc::sync_channel(1);
    let server = std::thread::spawn(move || {
        let (mut stream, _) = listener.accept().expect("accept request");
        let mut buffer = [0_u8; 4096];
        let read = stream.read(&mut buffer).unwrap_or(0);
        reached_tx
            .send(String::from_utf8_lossy(&buffer[..read]).into_owned())
            .expect("report request");
        // Deliberately not a valid provider envelope: this test is about the
        // transport reaching the network and framing its reply, not about the
        // reply parser, which has its own tests.
        let body = "nonsense";
        let _ = stream.write_all(
            format!(
                "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\n\r\n{body}",
                body.len()
            )
            .as_bytes(),
        );
        let _ = stream.flush();
    });

    let mut child = spawn_transport(&envelope(&format!("http://{address}")));
    let mut stdout = Vec::new();
    child
        .stdout
        .take()
        .expect("transport stdout")
        .read_to_end(&mut stdout)
        .expect("read transport stdout");
    let status = child.wait().expect("await transport child");

    let request = reached_rx
        .recv_timeout(Duration::from_secs(10))
        .expect("transport never reached the provider");
    server.join().expect("provider thread");

    assert!(request.starts_with("POST "), "request={request:?}");
    // The child is the one that made the request, and it answers with a single
    // framing byte followed by the exact provider envelope. Protocol parsing
    // belongs to the parent's PreparedAgentRequest, which still carries the
    // request's provider and Text/NativeTools selection; the transport must
    // not erase completion or tool-call metadata first.
    assert_eq!(stdout.first(), Some(&b'+'), "stdout={stdout:?}");
    assert_eq!(&stdout[1..], b"nonsense");
    assert!(status.success());
}

#[test]
fn a_malformed_envelope_is_reported_without_touching_the_network() {
    let mut child = spawn_transport("not an envelope");
    let mut stdout = Vec::new();
    child
        .stdout
        .take()
        .expect("transport stdout")
        .read_to_end(&mut stdout)
        .expect("read transport stdout");
    let status = child.wait().expect("await transport child");

    assert_eq!(stdout.first(), Some(&b'-'), "stdout={stdout:?}");
    assert!(
        String::from_utf8_lossy(&stdout).contains("malformed request"),
        "stdout={stdout:?}"
    );
    assert_eq!(status.code(), Some(1));
}

#[test]
fn a_transport_child_does_not_outlive_a_hard_killed_shell() {
    // Moving the request off a thread and into a process introduced one edge a
    // thread never had: every kill path needs a live parent to run it, so a
    // SIGKILLed shell could orphan a request that keeps the connection open.
    // PR_SET_PDEATHSIG closes that window.
    let listener = TcpListener::bind("127.0.0.1:0").expect("bind stalled provider");
    let address = listener.local_addr().expect("provider address");
    let (accepted_tx, accepted_rx) = mpsc::sync_channel(1);
    let (release_tx, release_rx) = mpsc::sync_channel::<()>(1);
    let server = std::thread::spawn(move || {
        let (stream, _) = listener.accept().expect("accept request");
        accepted_tx.send(()).expect("report accepted request");
        let _ = release_rx.recv_timeout(Duration::from_secs(30));
        drop(stream);
    });

    // A shell whose only job is to start the request and then be killed.
    let mut shell = Command::new(env!("CARGO_BIN_EXE_jsh"))
        .args(["-c", "agent inspect this repository"])
        .env("JSH_AI_PROVIDER", "ollama")
        .env("JSH_AI_BASE_URL", format!("http://{address}"))
        .env("NO_PROXY", "127.0.0.1,localhost")
        .env_remove("HTTP_PROXY")
        .env_remove("HTTPS_PROXY")
        .env_remove("ALL_PROXY")
        .env_remove("http_proxy")
        .env_remove("https_proxy")
        .env_remove("all_proxy")
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .expect("spawn shell");

    accepted_rx
        .recv_timeout(Duration::from_secs(15))
        .expect("shell never reached the stalled provider");

    // Positive control first. Asserting only that no transport survives would
    // pass just as happily if one had never started, which would make this test
    // prove nothing at all.
    let before = transport_pids();
    if before.is_empty() {
        let _ = shell.kill();
        let _ = shell.wait();
        let _ = release_tx.send(());
        let _ = server.join();
        panic!("no transport child was running while the provider stalled");
    }

    // SIGKILL: the shell gets no chance to clean up after itself.
    let pid = i32::try_from(shell.id()).expect("PID fits pid_t");
    assert_eq!(unsafe { nix::libc::kill(pid, nix::libc::SIGKILL) }, 0);
    shell.wait().expect("await killed shell");

    let deadline = Instant::now() + Duration::from_secs(10);
    while Instant::now() < deadline && !transport_pids().is_empty() {
        std::thread::sleep(Duration::from_millis(20));
    }
    let after = transport_pids();

    let _ = release_tx.send(());
    server.join().expect("stalled provider thread");
    assert!(
        after.is_empty(),
        "transport children outlived the shell that started them: {after:?}"
    );
}

/// PIDs of live transport children, read from `/proc` rather than matched
/// against `ps` output: a substring search finds the test harness's own command
/// line, which mentions the flag, and would report a child that does not exist.
fn transport_pids() -> Vec<String> {
    let mut found = Vec::new();
    let Ok(entries) = std::fs::read_dir("/proc") else {
        return found;
    };
    for entry in entries.flatten() {
        let name = entry.file_name();
        let Some(pid) = name.to_str() else { continue };
        if !pid.bytes().all(|byte| byte.is_ascii_digit()) {
            continue;
        }
        let Ok(cmdline) = std::fs::read(entry.path().join("cmdline")) else {
            continue;
        };
        let mut fields = cmdline.split(|byte| *byte == 0);
        let executable = fields.next().unwrap_or_default();
        let flag = fields.next().unwrap_or_default();
        if executable.ends_with(b"/jsh") && flag == FLAG.as_bytes() {
            found.push(pid.to_string());
        }
    }
    found
}

#[test]
fn killing_the_transport_ends_a_stalled_request_immediately() {
    // The regression this guards: with the request on a worker thread there was
    // nothing to kill, so a cancelled request stayed alive until the provider's
    // read timeout — 120 seconds — and blocked the next one for that whole
    // window. With a process, cancellation is a signal.
    let listener = TcpListener::bind("127.0.0.1:0").expect("bind stalled provider");
    let address = listener.local_addr().expect("provider address");
    let (accepted_tx, accepted_rx) = mpsc::sync_channel(1);
    let (release_tx, release_rx) = mpsc::sync_channel::<()>(1);
    let server = std::thread::spawn(move || {
        let (stream, _) = listener.accept().expect("accept request");
        accepted_tx.send(()).expect("report accepted request");
        // Hold the connection open and answer nothing at all.
        let _ = release_rx.recv_timeout(Duration::from_secs(30));
        drop(stream);
    });

    let mut child = spawn_transport(&envelope(&format!("http://{address}")));
    accepted_rx
        .recv_timeout(Duration::from_secs(10))
        .expect("transport never reached the stalled provider");

    let started = Instant::now();
    child.kill().expect("kill transport child");
    let status = child.wait().expect("await killed transport child");
    let elapsed = started.elapsed();

    let _ = release_tx.send(());
    server.join().expect("stalled provider thread");

    assert!(!status.success());
    assert!(
        elapsed < Duration::from_secs(5),
        "killing the transport took {elapsed:?}; it should be immediate"
    );
}
