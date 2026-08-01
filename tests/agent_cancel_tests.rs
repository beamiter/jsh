#![cfg(feature = "ai")]

use std::io::Read;
use std::net::TcpListener;
use std::process::{Command, Stdio};
use std::sync::mpsc;
use std::time::{Duration, Instant};

#[test]
fn sigint_cancels_an_agent_input_prompt() {
    let mut child = Command::new(env!("CARGO_BIN_EXE_jsh"))
        .args(["-c", "agent"])
        .env("JSH_AI_PROVIDER", "ollama")
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .expect("spawn jsh Agent prompt");
    // Keep stdin open: EOF must not be what releases the prompt.
    let _stdin = child.stdin.take().expect("Agent stdin");
    let mut stdout = child.stdout.take().expect("Agent stdout");
    let (ready_tx, ready_rx) = mpsc::sync_channel(1);
    let reader = std::thread::spawn(move || {
        let mut output = Vec::new();
        let mut byte = [0_u8; 1];
        let mut reported = false;
        loop {
            match stdout.read(&mut byte) {
                Ok(0) => break,
                Ok(_) => {
                    output.push(byte[0]);
                    if !reported && output.ends_with(b"agent goal> ") {
                        let _ = ready_tx.send(());
                        reported = true;
                    }
                }
                Err(error) if error.kind() == std::io::ErrorKind::Interrupted => continue,
                Err(_) => break,
            }
        }
        output
    });

    ready_rx
        .recv_timeout(Duration::from_secs(3))
        .expect("Agent prompt did not become ready");
    let pid = i32::try_from(child.id()).expect("PID fits pid_t");
    assert_eq!(unsafe { nix::libc::kill(pid, nix::libc::SIGINT) }, 0);

    let deadline = Instant::now() + Duration::from_secs(3);
    let status = loop {
        if let Some(status) = child.try_wait().expect("poll Agent child") {
            break status;
        }
        if Instant::now() >= deadline {
            let _ = child.kill();
            let _ = child.wait();
            panic!("Agent prompt did not stop after SIGINT");
        }
        std::thread::sleep(Duration::from_millis(10));
    };
    let output = reader.join().expect("stdout reader");
    let mut stderr = String::new();
    child
        .stderr
        .take()
        .expect("Agent stderr")
        .read_to_string(&mut stderr)
        .expect("read Agent stderr");

    assert_eq!(
        status.code(),
        Some(130),
        "stdout={output:?} stderr={stderr:?}"
    );
}

#[test]
fn sigint_cancels_an_agent_while_the_provider_stalls() {
    let listener = TcpListener::bind("127.0.0.1:0").expect("bind stalled provider");
    let address = listener.local_addr().expect("provider address");
    let (accepted_tx, accepted_rx) = mpsc::sync_channel(1);
    let (release_tx, release_rx) = mpsc::sync_channel(1);
    let server = std::thread::spawn(move || {
        let (_stream, _) = listener.accept().expect("accept Agent request");
        accepted_tx.send(()).expect("report accepted request");
        let _ = release_rx.recv_timeout(Duration::from_secs(5));
    });

    let mut child = Command::new(env!("CARGO_BIN_EXE_jsh"))
        .args(["-c", "agent inspect the repository; echo SHOULD_NOT_RUN"])
        // Loopback HTTP is intentionally limited to Ollama by the production
        // URL policy. The test only needs a provider socket that accepts the
        // request and then stalls; it must not weaken that policy to do so.
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
        .expect("spawn jsh Agent request");

    accepted_rx
        .recv_timeout(Duration::from_secs(3))
        .expect("Agent did not reach stalled provider");
    let pid = i32::try_from(child.id()).expect("PID fits pid_t");
    assert_eq!(unsafe { nix::libc::kill(pid, nix::libc::SIGINT) }, 0);

    let deadline = Instant::now() + Duration::from_secs(3);
    let status = loop {
        if let Some(status) = child.try_wait().expect("poll Agent child") {
            break status;
        }
        if Instant::now() >= deadline {
            let _ = child.kill();
            let _ = child.wait();
            let _ = release_tx.send(());
            let _ = server.join();
            panic!("Agent request did not stop after SIGINT");
        }
        std::thread::sleep(Duration::from_millis(10));
    };
    let _ = release_tx.send(());
    server.join().expect("stalled provider thread");
    let mut stdout = String::new();
    child
        .stdout
        .take()
        .expect("Agent stdout")
        .read_to_string(&mut stdout)
        .expect("read Agent stdout");
    let mut stderr = String::new();
    child
        .stderr
        .take()
        .expect("Agent stderr")
        .read_to_string(&mut stderr)
        .expect("read Agent stderr");

    assert_eq!(status.code(), Some(130), "stderr={stderr:?}");
    assert!(!stdout.contains("SHOULD_NOT_RUN"), "stdout={stdout:?}");
}
