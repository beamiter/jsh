//! Differential comparison against the system's bash.
//!
//! Every snippet under `tests/data/bash_probes/*.txt` (a `### name` line, then
//! the script) runs once under `bash --norc --noprofile -c` and once under
//! `jsh --norc -c`, each in a fresh temporary directory with the same small
//! environment. Standard output and exit status must match. Snippets whose
//! difference is deliberate or not yet closed are listed, with a reason, in
//! `tests/data/bash_probes/known_differences.txt`.
//!
//! The comparison depends on the host (bash version, installed programs,
//! timing), so it does not run by default:
//!
//! ```sh
//! cargo test --test bash_differential_tests -- --ignored
//! ```
use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::time::{Duration, Instant};

const PER_SCRIPT_TIMEOUT: Duration = Duration::from_secs(15);
const WORKERS: usize = 8;

struct Probe {
    file: String,
    name: String,
    script: String,
}

#[derive(Debug, PartialEq)]
struct Outcome {
    stdout: String,
    status: Option<i32>,
}

fn probe_dir() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/data/bash_probes")
}

fn parse_probes(file: &str, text: &str) -> Vec<Probe> {
    let mut probes = Vec::new();
    let mut current: Option<(String, String)> = None;
    for line in text.split_inclusive('\n') {
        if let Some(name) = line.strip_prefix("### ") {
            if let Some((name, script)) = current.take() {
                probes.push(Probe {
                    file: file.to_owned(),
                    name,
                    script,
                });
            }
            current = Some((name.trim().to_owned(), String::new()));
        } else if let Some((_, script)) = current.as_mut() {
            script.push_str(line);
        }
    }
    if let Some((name, script)) = current {
        probes.push(Probe {
            file: file.to_owned(),
            name,
            script,
        });
    }
    for probe in &mut probes {
        let trimmed = probe.script.trim_end_matches('\n').len();
        probe.script.truncate(trimmed);
        probe.script.push('\n');
    }
    probes
}

fn known_differences() -> BTreeMap<String, String> {
    let text = std::fs::read_to_string(probe_dir().join("known_differences.txt"))
        .expect("read known_differences.txt");
    text.lines()
        .map(str::trim)
        .filter(|line| !line.is_empty() && !line.starts_with('#'))
        .map(|line| match line.split_once(char::is_whitespace) {
            Some((name, reason)) => (name.to_owned(), reason.trim().to_owned()),
            None => (line.to_owned(), String::new()),
        })
        .collect()
}

fn wait_with_timeout(mut child: Child) -> Outcome {
    // Drain stdout on a thread so a chatty script cannot block on a full pipe
    // while we poll for its exit.
    let mut stdout = child.stdout.take().expect("piped stdout");
    let reader = std::thread::spawn(move || {
        let mut bytes = Vec::new();
        let _ = std::io::Read::read_to_end(&mut stdout, &mut bytes);
        bytes
    });
    let deadline = Instant::now() + PER_SCRIPT_TIMEOUT;
    let status = loop {
        match child.try_wait().expect("poll child") {
            Some(status) => break status.code(),
            None if Instant::now() >= deadline => {
                let _ = child.kill();
                let _ = child.wait();
                break Some(-999);
            }
            None => std::thread::sleep(Duration::from_millis(5)),
        }
    };
    let stdout = reader.join().unwrap_or_default();
    Outcome {
        stdout: String::from_utf8_lossy(&stdout).into_owned(),
        status,
    }
}

fn run_shell(program: &str, leading: &[&str], script: &str) -> Outcome {
    let dir = tempfile::tempdir().expect("tempdir");
    let child = Command::new(program)
        .args(leading)
        .arg(script)
        .current_dir(dir.path())
        .env_clear()
        .env("PATH", "/usr/local/bin:/usr/bin:/bin")
        .env("HOME", dir.path())
        .env("LANG", "C.UTF-8")
        .env("LC_ALL", "C.UTF-8")
        .env("TERM", "dumb")
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
        .expect("spawn shell");
    wait_with_timeout(child)
}

fn system_bash() -> Option<String> {
    for candidate in ["/bin/bash", "/usr/bin/bash"] {
        let Ok(output) = Command::new(candidate).arg("--version").output() else {
            continue;
        };
        let banner = String::from_utf8_lossy(&output.stdout);
        // Probes are written against bash 5.x semantics.
        if banner.contains("version 5.") {
            return Some(candidate.to_owned());
        }
    }
    None
}

#[test]
#[ignore = "compares with the host's bash; run with --ignored"]
fn jsh_matches_bash_on_the_probe_corpus() {
    let Some(bash) = system_bash() else {
        eprintln!("skipping: no bash 5.x at /bin/bash or /usr/bin/bash");
        return;
    };
    let mut probes = Vec::new();
    let mut entries: Vec<_> = std::fs::read_dir(probe_dir())
        .expect("read probe directory")
        .map(|entry| entry.expect("probe entry").path())
        .filter(|path| path.extension().is_some_and(|ext| ext == "txt"))
        .filter(|path| !path.ends_with("known_differences.txt"))
        .collect();
    entries.sort();
    for path in entries {
        let file = path.file_name().unwrap().to_string_lossy().into_owned();
        let text = std::fs::read_to_string(&path).expect("read probe file");
        probes.extend(parse_probes(&file, &text));
    }
    assert!(!probes.is_empty(), "no probes found");
    let known = known_differences();

    let jsh = env!("CARGO_BIN_EXE_jsh");
    let chunk = probes.len().div_ceil(WORKERS);
    let results: Vec<(String, String, Outcome, Outcome)> = std::thread::scope(|scope| {
        let handles: Vec<_> = probes
            .chunks(chunk)
            .map(|slice| {
                let bash = bash.clone();
                scope.spawn(move || {
                    slice
                        .iter()
                        .map(|probe| {
                            let expected =
                                run_shell(&bash, &["--norc", "--noprofile", "-c"], &probe.script);
                            let actual = run_shell(jsh, &["--norc", "-c"], &probe.script);
                            (probe.file.clone(), probe.name.clone(), expected, actual)
                        })
                        .collect::<Vec<_>>()
                })
            })
            .collect();
        handles
            .into_iter()
            .flat_map(|handle| handle.join().expect("probe worker"))
            .collect()
    });

    let mut unexpected = Vec::new();
    let mut now_matching = Vec::new();
    for (file, name, expected, actual) in &results {
        let differs = expected != actual;
        match (differs, known.contains_key(name)) {
            (true, false) => unexpected.push(format!(
                "{file}::{name}\n  bash: status {:?}, stdout {:?}\n  jsh:  status {:?}, stdout {:?}",
                expected.status, expected.stdout, actual.status, actual.stdout
            )),
            (false, true) => now_matching.push(name.clone()),
            _ => {}
        }
    }
    let matched = results.iter().filter(|(_, _, e, a)| e == a).count();
    eprintln!(
        "{matched}/{} probes match bash; {} listed as known differences",
        results.len(),
        known.len()
    );
    if !now_matching.is_empty() {
        eprintln!(
            "known differences that now match (remove them from the list): {}",
            now_matching.join(", ")
        );
    }
    assert!(
        unexpected.is_empty(),
        "{} unexpected difference(s):\n{}",
        unexpected.len(),
        unexpected.join("\n")
    );
}

#[test]
fn probe_files_parse_and_names_are_unique() {
    let mut seen = BTreeMap::new();
    for entry in std::fs::read_dir(probe_dir()).expect("read probe directory") {
        let path = entry.expect("probe entry").path();
        if path.extension().is_none_or(|ext| ext != "txt")
            || path.ends_with("known_differences.txt")
        {
            continue;
        }
        let file = path.file_name().unwrap().to_string_lossy().into_owned();
        let text = std::fs::read_to_string(&path).expect("read probe file");
        for probe in parse_probes(&file, &text) {
            assert!(
                !probe.script.trim().is_empty(),
                "{file}::{} is empty",
                probe.name
            );
            if let Some(other) = seen.insert(probe.name.clone(), file.clone()) {
                panic!("probe {} appears in both {other} and {file}", probe.name);
            }
        }
    }
    assert!(!seen.is_empty());
    for name in known_differences().keys() {
        assert!(
            seen.contains_key(name),
            "known_differences.txt lists unknown probe {name}"
        );
    }
}
