//! Exercise a mock HTTP model through the actual jsh/jagent executable.
#[test]
fn model_driven_research_loop_end_to_end() {
    let output = std::process::Command::new("python3")
        .current_dir(env!("CARGO_MANIFEST_DIR"))
        .env("JSH_RESEARCH_BINARY", env!("CARGO_BIN_EXE_jsh"))
        .args(["-m", "unittest", "discover", "-s", "tests", "-p", "test_research_auto.py", "-v"])
        .output().expect("Python 3.10+ is required for research tests");
    assert!(output.status.success(), "stdout:\n{}\nstderr:\n{}", String::from_utf8_lossy(&output.stdout), String::from_utf8_lossy(&output.stderr));
}
