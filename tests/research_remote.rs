//! Exercise the actual jsh binary, shared jagent parser and Python controller.
#[test]
fn research_remote_gateway_integration() {
    let output = std::process::Command::new("python3")
        .current_dir(env!("CARGO_MANIFEST_DIR"))
        .env("JSH_RESEARCH_BINARY", env!("CARGO_BIN_EXE_jsh"))
        .args([
            "-m",
            "unittest",
            "discover",
            "-s",
            "tests",
            "-p",
            "test_research_remote.py",
            "-v",
        ])
        .output()
        .expect("Python 3.10+ is required by the research integration suite");
    assert!(
        output.status.success(),
        "stdout:\n{}\nstderr:\n{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
}
