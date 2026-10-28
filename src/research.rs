//! Model-independent research orchestration, available without the AI feature.
//!
//! Embed the standard-library-only Python controller so installed binaries do
//! not depend on the source checkout. Python 3.10+ is required only for research;
//! ordinary shell startup never starts or probes Python. The local executor is
//! not a security sandbox: review docs/autoresearch.md before authorizing code.

use std::process::Command;

const ENGINE: &str = include_str!("../scripts/jsh_research.py");

/// Dispatch `jsh research ...`, preserving arguments without shell evaluation.
pub fn entrypoint() -> i32 {
    let mut command = Command::new("python3");
    command
        .args(["-I", "-X", "utf8", "-c", ENGINE])
        .args(std::env::args_os().skip(2));

    #[cfg(unix)]
    {
        // Replace the wrapper so Ctrl-C/SIGTERM reach the controller directly.
        use std::os::unix::process::CommandExt;
        let error = command.exec();
        eprintln!("jsh research: cannot start python3 (requires Python 3.10+): {error}");
        if error.kind() == std::io::ErrorKind::NotFound {
            127
        } else {
            126
        }
    }
    #[cfg(not(unix))]
    {
        eprintln!("jsh research: the local executor currently requires Unix");
        2
    }
}
