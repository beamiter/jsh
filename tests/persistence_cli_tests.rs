use std::fs;
use std::os::unix::fs::{symlink, PermissionsExt};
use std::path::Path;
use std::process::{Command, Output};

fn run(home: &Path, command: &str) -> Output {
    Command::new(env!("CARGO_BIN_EXE_jsh"))
        .args(["--norc", "-c", command])
        .env("HOME", home)
        .output()
        .expect("run shell with isolated persistence")
}

fn blocked_store(home: &Path, name: &str, contents: &str) {
    let path = home.join(name);
    fs::write(&path, contents).unwrap();
    fs::set_permissions(&path, fs::Permissions::from_mode(0o600)).unwrap();
    let outside = home.join(format!("{name}.outside"));
    fs::write(&outside, b"do not change").unwrap();
    symlink(outside, home.join(format!("{name}.lock"))).unwrap();
}

#[test]
fn bookmark_add_reports_persistence_failure_instead_of_success() {
    let home = tempfile::tempdir().unwrap();
    blocked_store(home.path(), ".jsh_bookmarks", "existing|/original\n");
    let result = run(home.path(), "bookmark add new /new");
    assert_eq!(result.status.code(), Some(1));
    assert!(result.stdout.is_empty());
    assert!(String::from_utf8_lossy(&result.stderr).contains("bookmark save failed"));
    assert_eq!(
        fs::read_to_string(home.path().join(".jsh_bookmarks")).unwrap(),
        "existing|/original\n"
    );
}

#[test]
fn bookmark_remove_distinguishes_write_failure_from_missing_name() {
    let home = tempfile::tempdir().unwrap();
    blocked_store(home.path(), ".jsh_bookmarks", "existing|/original\n");
    let result = run(home.path(), "bookmark rm existing");
    let error = String::from_utf8_lossy(&result.stderr);
    assert_eq!(result.status.code(), Some(1));
    assert!(result.stdout.is_empty());
    assert!(error.contains("could not persist removal"), "{error}");
    assert!(!error.contains("not found"), "{error}");
    assert_eq!(
        fs::read_to_string(home.path().join(".jsh_bookmarks")).unwrap(),
        "existing|/original\n"
    );
}

#[test]
fn z_remove_modes_report_persistence_failures() {
    let home = tempfile::tempdir().unwrap();
    blocked_store(home.path(), ".jsh_z", "/original|4|123\n");
    for command in ["z -x /original", "z -c"] {
        let result = run(home.path(), command);
        let error = String::from_utf8_lossy(&result.stderr);
        assert_eq!(result.status.code(), Some(1), "{command}: {error}");
        assert!(error.contains("could not persist removal"), "{error}");
    }
    assert_eq!(
        fs::read_to_string(home.path().join(".jsh_z")).unwrap(),
        "/original|4|123\n"
    );
}
