//! A two-row handshake proves that a row is visible before the producer can
//! make progress. No large source, scheduling sleep, or throughput assumption.

use jsh::pipeline_data::PipelineData;
use jsh::value::Value;
use std::io::{self, BufRead, Read, Write};
use std::process::{Child, Command, Stdio};
use std::sync::mpsc::{self, Receiver};
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

const CHILD_MODE: &str = "JSH_TEST_STREAM_OUTPUT_CHILD";
const FIRST_ROW: &str = "jsh-stream-output-first-row-861f725a";
const SECOND_ROW: &str = "jsh-stream-output-second-row-861f725a";
const TIMEOUT: Duration = Duration::from_secs(10);

struct HelperProcess {
    child: Child,
    reader: Option<JoinHandle<io::Result<()>>>,
}

impl Drop for HelperProcess {
    fn drop(&mut self) {
        // This also runs on assertion failure or timeout while the child is
        // waiting for ACK. Reap it before joining the stdout reader.
        let _ = self.child.kill();
        let _ = self.child.wait();
        if let Some(reader) = self.reader.take() {
            let _ = reader.join();
        }
    }
}

fn receive_until(
    receiver: &Receiver<Option<String>>,
    marker: Option<&str>,
    transcript: &mut String,
) {
    let deadline = Instant::now() + TIMEOUT;
    loop {
        let event = receiver
            .recv_timeout(deadline.saturating_duration_since(Instant::now()))
            .unwrap_or_else(|error| {
                panic!("stream output did not reach {marker:?}: {error}; output={transcript:?}")
            });
        match event {
            Some(line) => {
                transcript.push_str(&line);
                // libtest may write its test-name prefix before the first row.
                if marker.is_some_and(|marker| line.trim_end().ends_with(marker)) {
                    return;
                }
            }
            None if marker.is_none() => return,
            None => panic!("helper exited before {marker:?}; output={transcript:?}"),
        }
    }
}

fn child_stream() {
    let mut stage = 0;
    let values = std::iter::from_fn(move || match stage {
        0 => {
            stage = 1;
            Some(Value::String(FIRST_ROW.to_owned()))
        }
        1 => {
            let mut acknowledgement = [0];
            io::stdin()
                .read_exact(&mut acknowledgement)
                .expect("parent must acknowledge the visible first row");
            assert_eq!(acknowledgement, *b"A");
            stage = 2;
            Some(Value::String(SECOND_ROW.to_owned()))
        }
        _ => None,
    });
    PipelineData::Stream(Box::new(values))
        .write_to_stdout()
        .expect("write stream rows");
}

#[test]
fn each_stream_row_is_visible_before_requesting_the_next_row() {
    if std::env::var(CHILD_MODE).as_deref() == Ok("1") {
        child_stream();
        return;
    }

    let child = Command::new(std::env::current_exe().expect("current test executable"))
        .args([
            "--exact",
            "each_stream_row_is_visible_before_requesting_the_next_row",
            "--nocapture",
            "--test-threads=1",
        ])
        .env(CHILD_MODE, "1")
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::inherit())
        .spawn()
        .expect("spawn stream-output helper");
    let mut helper = HelperProcess {
        child,
        reader: None,
    };
    let stdout = helper.child.stdout.take().expect("helper stdout");
    let (sender, receiver) = mpsc::channel();
    helper.reader = Some(std::thread::spawn(move || {
        let mut stdout = io::BufReader::new(stdout);
        loop {
            let mut line = String::new();
            if stdout.read_line(&mut line)? == 0 {
                let _ = sender.send(None);
                return Ok(());
            }
            if sender.send(Some(line)).is_err() {
                return Ok(());
            }
        }
    }));

    let mut transcript = String::new();
    receive_until(&receiver, Some(FIRST_ROW), &mut transcript);
    // The producer cannot finish its stream before this acknowledgement. A
    // whole-stream or block buffer therefore deadlocks until the test timeout.
    let mut stdin = helper.child.stdin.take().expect("helper stdin");
    stdin.write_all(b"A").expect("acknowledge first row");
    stdin.flush().expect("flush acknowledgement");
    drop(stdin);
    receive_until(&receiver, Some(SECOND_ROW), &mut transcript);
    receive_until(&receiver, None, &mut transcript);
    let status = helper.child.wait().expect("wait for stream-output helper");
    assert!(status.success(), "{status}: {transcript}");
    helper
        .reader
        .take()
        .unwrap()
        .join()
        .expect("reader thread")
        .expect("read helper output");
}
