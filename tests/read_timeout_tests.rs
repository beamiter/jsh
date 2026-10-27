use std::io::{Read, Write};
use std::process::{Child, Command, Output, Stdio};
use std::time::{Duration, Instant};

fn spawn_read(options: &str) -> Child {
    Command::new(env!("CARGO_BIN_EXE_jsh"))
        .args([
            "--norc",
            "-c",
            &format!(r#"value=old; read {options} value; printf '%s:<%s>' "$?" "$value""#),
        ])
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .expect("spawn read fixture")
}

fn finish_promptly(mut child: Child) -> Output {
    let deadline = Instant::now() + Duration::from_secs(2);
    loop {
        if child.try_wait().expect("poll read fixture").is_some() {
            return child.wait_with_output().expect("capture read fixture");
        }
        if Instant::now() >= deadline {
            let _ = child.kill();
            let _ = child.wait();
            panic!("read did not honor its timeout while stdin stayed open");
        }
        std::thread::sleep(Duration::from_millis(5));
    }
}

#[test]
fn fractional_timeout_finishes_without_stdin_eof_in_every_read_mode() {
    for options in ["-t 0.05", "-t 0.05 -n 10", "-t 0.05 -N 10"] {
        let mut child = spawn_read(options);
        let _input = child.stdin.take().expect("keep stdin open");
        let output = finish_promptly(child);
        assert_eq!(output.status.code(), Some(0));
        assert_eq!(output.stdout, b"142:<>", "{options}: {:?}", output.stderr);
    }
}

#[test]
fn timeout_preserves_partial_input_in_every_read_mode() {
    for options in ["-r -t 0.05", "-t 0.05 -n 10", "-t 0.05 -N 10"] {
        let mut child = spawn_read(options);
        let mut input = child.stdin.take().unwrap();
        input.write_all(b"part\r").unwrap();
        input.flush().unwrap();
        let output = finish_promptly(child);
        assert_eq!(output.stdout, b"142:<part\r>", "{options}");
    }
}

#[test]
fn zero_timeout_checks_readiness_without_consuming_or_assigning() {
    let mut child = spawn_read("-t 0");
    let _input = child.stdin.take().unwrap();
    assert_eq!(finish_promptly(child).stdout, b"1:<old>");

    for value in ["0", "-0", "+0.0"] {
        // Fill and close the pipe before jsh starts. Writing after spawn raced
        // the readiness check itself: on a loaded machine `read -t 0` could
        // run first, correctly report "not ready" (1), and fail the test.
        let (reader, mut writer) = std::io::pipe().unwrap();
        writer.write_all(b"untouched\n").unwrap();
        drop(writer);
        let child = Command::new(env!("CARGO_BIN_EXE_jsh"))
            .args(["--norc", "-c", &format!(
                r#"value=old; read -t '{value}' value; printf '%s:<%s>' "$?" "$value"; /usr/bin/cat"#
            )])
            .stdin(reader).stdout(Stdio::piped()).stderr(Stdio::piped())
            .spawn().unwrap();
        assert_eq!(finish_promptly(child).stdout, b"0:<old>untouched\n");
    }
}

#[test]
fn invalid_timeout_is_rejected_without_consuming_input() {
    let tiny_negative = format!("-0.{}1", "0".repeat(400));
    for value in [
        "-1",
        "NaN",
        "inf",
        "1e-3",
        "1.2.3",
        " 1",
        "1 ",
        "9999999999999999999999999999999",
        tiny_negative.as_str(),
    ] {
        let mut child = Command::new(env!("CARGO_BIN_EXE_jsh"))
            .args(["--norc", "-c", &format!(
                r#"value=old; read -t '{value}' value; printf '%s:<%s>' "$?" "$value"; /usr/bin/cat"#
            )])
            .stdin(Stdio::piped()).stdout(Stdio::piped()).stderr(Stdio::piped())
            .spawn().unwrap();
        child
            .stdin
            .take()
            .unwrap()
            .write_all(b"untouched\n")
            .unwrap();
        assert_eq!(
            finish_promptly(child).stdout,
            b"1:<old>untouched\n",
            "{value}"
        );
    }
    let output = Command::new(env!("CARGO_BIN_EXE_jsh"))
        .args(["--norc", "-c", "read -t"])
        .output()
        .unwrap();
    assert_eq!(output.status.code(), Some(2));
}

#[test]
fn incoming_bytes_do_not_restart_the_absolute_deadline() {
    for options in ["-t 0.12", "-t 0.12 -n 20", "-t 0.12 -N 20"] {
        let mut child = spawn_read(options);
        let mut input = child.stdin.take().unwrap();
        let writer = std::thread::spawn(move || {
            for _ in 0..10 {
                if input.write_all(b"x").is_err() {
                    break;
                }
                std::thread::sleep(Duration::from_millis(40));
            }
        });
        let output = finish_promptly(child);
        writer.join().unwrap();
        assert!(
            output.stdout.starts_with(b"142:<"),
            "{options}: {:?}",
            output.stdout
        );
        let count = output.stdout.iter().filter(|&&byte| byte == b'x').count();
        assert!(count > 0 && count < 10, "{options}: {count}");
    }
}

#[test]
fn limited_reads_wait_for_the_count_or_delimiter() {
    let mut child = spawn_read("-t 0.5 -n 10");
    let mut input = child.stdin.take().unwrap();
    input.write_all(b"first").unwrap();
    input.flush().unwrap();
    std::thread::sleep(Duration::from_millis(30));
    input.write_all(b"\ntrailing").unwrap();
    input.flush().unwrap();
    let output = finish_promptly(child);
    assert_eq!(output.stdout, b"0:<first>");
}

#[test]
fn harmless_signals_do_not_extend_timeout_and_termination_wins() {
    use nix::sys::signal::{kill, Signal};
    use nix::unistd::Pid;
    for options in ["-t 0.15", "-t 0.15 -n 20", "-t 0.15 -N 20"] {
        for signal in [Signal::SIGWINCH, Signal::SIGTERM, Signal::SIGINT] {
            let mut child = spawn_read(options);
            let mut input = child.stdin.take().unwrap();
            input.write_all(b"x").unwrap();
            input.flush().unwrap();
            let pid = Pid::from_raw(child.id() as i32);
            let started = Instant::now();
            let stop = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
            let stop_sender = stop.clone();
            let sender = std::thread::spawn(move || {
                std::thread::sleep(Duration::from_millis(30));
                for _ in 0..10 {
                    if stop_sender.load(std::sync::atomic::Ordering::SeqCst)
                        || kill(pid, signal).is_err()
                    {
                        break;
                    }
                    std::thread::sleep(Duration::from_millis(35));
                }
            });
            let output = finish_promptly(child);
            let elapsed = started.elapsed();
            stop.store(true, std::sync::atomic::Ordering::SeqCst);
            sender.join().unwrap();
            assert!(
                elapsed < Duration::from_millis(300),
                "signal restarted deadline: {elapsed:?}"
            );
            match signal {
                Signal::SIGWINCH => assert_eq!(output.stdout, b"142:<x>"),
                Signal::SIGTERM => assert_eq!(output.status.code(), Some(143)),
                Signal::SIGINT => assert_eq!(output.status.code(), Some(130)),
                _ => unreachable!(),
            }
        }
    }
}

#[test]
fn bytes_arriving_after_timeout_remain_for_the_next_command() {
    use std::io::BufRead;
    use std::sync::mpsc;
    let mut child = Command::new(env!("CARGO_BIN_EXE_jsh"))
        .args([
            "--norc",
            "-c",
            r#"read -t 0.05 value; printf '%s:<%s>\n' "$?" "$value"; /usr/bin/cat"#,
        ])
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    let mut input = child.stdin.take().unwrap();
    input.write_all(b"part").unwrap();
    input.flush().unwrap();
    let stdout = child.stdout.take().unwrap();
    let (sender, receiver) = mpsc::channel();
    let reader = std::thread::spawn(move || {
        let mut stdout = std::io::BufReader::new(stdout);
        let mut first = String::new();
        stdout.read_line(&mut first).unwrap();
        sender.send(first).unwrap();
        let mut remaining = Vec::new();
        stdout.read_to_end(&mut remaining).unwrap();
        remaining
    });
    let first = receiver
        .recv_timeout(Duration::from_secs(2))
        .unwrap_or_else(|_| {
            let _ = child.kill();
            let _ = child.wait();
            panic!("read did not report timeout before later input");
        });
    assert_eq!(first, "142:<part>\n");
    input.write_all(b"later\n").unwrap();
    drop(input);
    assert!(finish_promptly(child).status.success());
    assert_eq!(reader.join().unwrap(), b"later\n");
}
