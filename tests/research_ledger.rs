//! Check persisted accounting and recovery through the embedded controller.
#[test]
fn research_ledger_cross_language_integration() {
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
            "test_research_ledger.py",
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
