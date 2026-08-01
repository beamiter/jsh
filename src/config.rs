/// Config file loading: source ~/.bashrc or ~/.jshrc on startup.
///
/// This module also owns the one-way migration of pre-rename `rsh` user data
/// (see [`migrate_legacy_rsh_data`]), because that migration is part of
/// startup-time file handling and has to run before any subsystem reads its
/// own data file.
use crate::environment::{ConfigSource, ShellState};
use crate::executor;
use crate::parser;
use std::fs::{self, File, OpenOptions};
use std::io;
use std::os::unix::fs::{OpenOptionsExt, PermissionsExt};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};

const MAX_STARTUP_FILE_BYTES: usize = 8 * 1024 * 1024;
const MAX_CONFIG_HELPER_STDOUT_BYTES: usize = 8 * 1024 * 1024;
const MAX_CONFIG_HELPER_STDERR_BYTES: usize = 512 * 1024;
const CONFIG_HELPER_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(30);

pub fn load_config(state: &mut ShellState) {
    // Startup may source a ~/.jshrc that only exists once the pre-rename
    // ~/.rshrc has been copied across.
    migrate_legacy_rsh_data();
    match state.shell_opts.config_source {
        ConfigSource::Bashrc => load_bashrc(state),
        ConfigSource::Jshrc => load_jshrc(state),
    }
}

pub fn refresh_shell_integrations(state: &mut ShellState) {
    load_conda_hook(state);
}

/// Load an explicitly selected startup file.
///
/// Native jsh syntax is attempted first. Files using syntax that jsh cannot
/// parse are imported through the same Bash compatibility bridge as `.bashrc`.
pub fn load_config_file(path: &Path, state: &mut ShellState) {
    source_file_lenient(path, state);
}

/// Load ~/.bashrc directly via bash, without attempting jsh parsing
fn load_bashrc(state: &mut ShellState) {
    let bashrc = state.home_dir.join(".bashrc");
    if bashrc.exists() {
        source_via_bash(&bashrc, state);
    }
}

/// Load ~/.jshrc via jsh parser with bash fallback for complex scripts
fn load_jshrc(state: &mut ShellState) {
    let jshrc = state.home_dir.join(".jshrc");
    if jshrc.exists() {
        source_file_lenient(&jshrc, state);
    }
}

/// Load bash file with lenient error handling - use bash as fallback for complex scripts
fn source_file_lenient(path: &Path, state: &mut ShellState) {
    let Ok(content) = crate::io_guard::read_regular_text(path, MAX_STARTUP_FILE_BYTES) else {
        return; // Missing default startup files are normal.
    };
    // Try parsing the entire file first.
    match parser::parse(&content) {
        Ok(commands) => {
            // Execute the startup file as one program so exit, failglob, and
            // errexit stop the remaining rc commands.
            executor::execute_program(&commands, state);
        }
        Err(_) => {
            // Full parse failed, use bash as fallback for complex scripts.
            source_via_bash(path, state);
        }
    }
}

/// Use bash to source a script file and extract environment variables, aliases, functions, and options
fn source_via_bash(path: &Path, state: &mut ShellState) {
    // `$1` transports the path as data. Interpolating it into this program would
    // make quotes, command substitutions, or newlines in a filename executable.
    let bash_script = r#"
# Set PS1 to make bash think it's interactive (some .bashrc check [ -z "$PS1" ] && return)
export PS1='$ '

set -a
source -- "$1"
set +a

# Output all environment variables in key=value format
echo "=== ENV_VARS ==="
declare -p | grep 'declare -x' | sed 's/declare -x //'

# Output aliases
echo "=== ALIASES ==="
alias -p 2>/dev/null || true

# Output function names
echo "=== FUNCTIONS ==="
declare -F 2>/dev/null | awk '{{print $3}}' || true

# Output shell options (shopt)
echo "=== SHOPTS ==="
shopt 2>/dev/null || true
"#;

    // Execute bash script to capture the environment, aliases, and functions
    let Some(bash) = crate::io_guard::automatic_system_helper("bash") else {
        eprintln!("jsh: Bash startup import unavailable: no trusted system Bash");
        return;
    };
    let mut command = std::process::Command::new(bash);
    command
        .arg("-c")
        .arg(bash_script)
        .arg("jsh-config")
        .arg(path);
    match crate::io_guard::bounded_command_output(
        &mut command,
        MAX_CONFIG_HELPER_STDOUT_BYTES,
        MAX_CONFIG_HELPER_STDERR_BYTES,
        CONFIG_HELPER_TIMEOUT,
    ) {
        Ok(output) => parse_bash_output(&String::from_utf8_lossy(&output.stdout), state),
        Err(error) => eprintln!("jsh: bash startup import failed for {path:?}: {error}"),
    }
}

/// If conda is present after importing bashrc state, load its POSIX shell hook
/// into the current jsh process so `conda activate` works interactively.
fn load_conda_hook(state: &mut ShellState) {
    let Some(conda_cmd) = state
        .get_var("CONDA_EXE")
        .filter(|s| !s.is_empty())
        .map(PathBuf::from)
    else {
        return;
    };
    if !crate::io_guard::explicit_absolute_executable(&conda_cmd) {
        return;
    }

    let mut command = std::process::Command::new(&conda_cmd);
    command.args(["shell.posix", "hook"]);
    let output = match crate::io_guard::bounded_command_output(
        &mut command,
        MAX_CONFIG_HELPER_STDOUT_BYTES,
        MAX_CONFIG_HELPER_STDERR_BYTES,
        CONFIG_HELPER_TIMEOUT,
    ) {
        Ok(output) if output.status.success() => output,
        _ => return,
    };

    let hook = String::from_utf8_lossy(&output.stdout);
    if hook.trim().is_empty() {
        return;
    }

    if let Ok(commands) = parser::parse(&hook) {
        executor::execute_program(&commands, state);
    }
}

/// Parse bash output containing env vars, aliases, functions, and shopt settings
fn parse_bash_output(output: &str, state: &mut ShellState) {
    let mut current_section = "";

    for line in output.lines() {
        match line {
            "=== ENV_VARS ===" => {
                current_section = "ENV_VARS";
                continue;
            }
            "=== ALIASES ===" => {
                current_section = "ALIASES";
                continue;
            }
            "=== FUNCTIONS ===" => {
                current_section = "FUNCTIONS";
                continue;
            }
            "=== SHOPTS ===" => {
                current_section = "SHOPTS";
                continue;
            }
            _ => {}
        }

        match current_section {
            "ENV_VARS" => {
                if line.is_empty() {
                    continue;
                }
                if let Some(eq_pos) = line.find('=') {
                    let key = &line[..eq_pos];
                    let value = &line[eq_pos + 1..];
                    // Remove quotes if present
                    let value = if (value.starts_with('\'') && value.ends_with('\''))
                        || (value.starts_with('"') && value.ends_with('"'))
                    {
                        &value[1..value.len() - 1]
                    } else {
                        value
                    };
                    state.export_var(key, value);
                }
            }
            "ALIASES" => {
                if line.is_empty() || !line.starts_with("alias ") {
                    continue;
                }
                // Parse "alias name='value'" format
                let alias_def = &line[6..]; // skip "alias "
                if let Some(eq_pos) = alias_def.find('=') {
                    let name = &alias_def[..eq_pos];
                    let value = &alias_def[eq_pos + 1..];
                    // Remove surrounding quotes
                    let value = value.trim_matches('\'').trim_matches('"');
                    state.aliases.insert(name.to_string(), value.to_string());
                }
            }
            "FUNCTIONS" => {
                if !line.is_empty() {
                    // For now, just note that functions are defined
                    // Full function body parsing requires additional bash invocation
                    // Functions will be stored once we implement full parsing
                }
            }
            "SHOPTS" => {
                if line.is_empty() {
                    continue;
                }
                // Parse shopt output: "shopt_name     on/off"
                let parts: Vec<&str> = line.split_whitespace().collect();
                if parts.len() >= 2 {
                    let opt_name = parts[0];
                    let opt_value = parts[parts.len() - 1];
                    let enabled = opt_value == "on";

                    // Map bash shopt names to jsh ShellOpts. Names jsh does not
                    // model are remembered rather than dropped, so `shopt
                    // histappend` answers the same in jsh as in the bash the
                    // settings came from.
                    if crate::builtins::shopt_option_is_known(opt_name) {
                        crate::builtins::set_shopt_option(state, opt_name, enabled);
                    }
                }
            }
            _ => {}
        }
    }
}

// ─── Legacy `rsh` → `jsh` data migration ────────────────────────────────────
//
// The 0.2.0 rename moved every user-data path at once (~/.rshrc → ~/.jshrc,
// ~/.rsh_history → ~/.jsh_history, ~/.rsh/ → ~/.jsh/, ~/.local/state/rsh →
// ~/.local/state/jsh, …) with no migration, so the first start of the renamed
// binary looked like a factory reset: history, bookmarks, rc file, saved
// sessions and the execution journal all appeared to be gone. History is the
// one users notice and cannot reconstruct.
//
// The rules below exist because the failure this fixes is *data loss*, so the
// migration must not be able to cause any:
//   * copy, never move — an installed `rsh` binary may still be in use, and a
//     failed copy must not have eaten the original;
//   * create-new only — the destination is materialised through link(2), which
//     fails with EEXIST rather than overwriting, so newer jsh data can never be
//     clobbered even if it appears mid-migration;
//   * every error is a warning, never fatal — a shell must still start.

/// Directory tree copies stop here. The migrated trees (sessions,
/// completions, workflows) are one level deep; the bound only exists so a
/// symlink loop or a pathological tree cannot stall startup.
const MAX_LEGACY_COPY_DEPTH: u32 = 8;

static LEGACY_MIGRATION_DONE: AtomicBool = AtomicBool::new(false);

/// What one migration pass did, so callers (and tests) can report it.
#[derive(Debug, Default)]
pub struct LegacyMigration {
    /// Human-readable `old -> new` descriptions of files actually copied.
    pub migrated: Vec<String>,
    /// Non-fatal problems: unreadable sources, refused copies, odd file types.
    pub warnings: Vec<String>,
}

impl LegacyMigration {
    pub fn is_empty(&self) -> bool {
        self.migrated.is_empty() && self.warnings.is_empty()
    }
}

/// Pre-rename location of the history file, for diagnostics elsewhere.
pub fn legacy_history_path(home: &Path) -> PathBuf {
    home.join(".rsh_history")
}

/// Copy pre-rename `rsh` user data into the `jsh` locations, at most once per
/// process. Called from every subsystem that opens a default data path, so
/// whichever one touches disk first performs the whole pass; that keeps the
/// migration independent of any single startup hook.
pub fn migrate_legacy_rsh_data() {
    // Unit tests run against the developer's real $HOME. Migrating there would
    // write outside the test's scratch directory, so the implicit pass is
    // disabled under `cargo test`; tests call `migrate_legacy_rsh_data_in`
    // with an isolated root, and tests/legacy_rsh_migration_tests.rs covers
    // the real startup path by spawning the binary with HOME set.
    if cfg!(test) {
        return;
    }
    if LEGACY_MIGRATION_DONE.swap(true, Ordering::SeqCst) {
        return;
    }
    let Some(home) = dirs::home_dir() else {
        return;
    };
    let state_dir = dirs::state_dir().unwrap_or_else(|| home.join(".local").join("state"));
    let report = migrate_legacy_rsh_data_in(&home, &state_dir);
    report_legacy_migration(&report);
}

/// Emit the single info line a bug report needs, plus any warnings.
fn report_legacy_migration(report: &LegacyMigration) {
    if !report.migrated.is_empty() {
        eprintln!(
            "jsh: migrated {} pre-rename rsh file(s): {} (the rsh copies were left in place)",
            report.migrated.len(),
            report.migrated.join(", ")
        );
    }
    for warning in &report.warnings {
        eprintln!("jsh: warning: legacy rsh migration: {warning}");
    }
}

/// The migration proper, parameterised on its roots so it is testable with an
/// isolated HOME. Idempotent: a second pass finds every destination present
/// and reports nothing.
pub fn migrate_legacy_rsh_data_in(home: &Path, state_dir: &Path) -> LegacyMigration {
    let mut report = LegacyMigration::default();

    // Flat dotfiles. `.rsh_z` is zjump's frecency database: it is not this
    // module's data, but the rename moved it too and copying it needs no
    // change in zjump.rs.
    for (old_name, new_name) in [
        (".rshrc", ".jshrc"),
        (".rsh_history", ".jsh_history"),
        (".rsh_bookmarks", ".jsh_bookmarks"),
        (".rsh_z", ".jsh_z"),
    ] {
        copy_legacy_file(&home.join(old_name), &home.join(new_name), &mut report);
    }

    // ~/.rsh/ holds sessions/, completions/ and workflows/. Copying the tree
    // rather than an enumerated list means data added by a sibling module does
    // not get left behind.
    copy_legacy_tree(&home.join(".rsh"), &home.join(".jsh"), 0, &mut report);

    // The execution journal is the only path that honours XDG_STATE_HOME.
    copy_legacy_tree(
        &state_dir.join("rsh"),
        &state_dir.join("jsh"),
        0,
        &mut report,
    );

    report
}

/// Recursively copy `old` into `new`, file by file, skipping anything that
/// already exists on the new side.
fn copy_legacy_tree(old: &Path, new: &Path, depth: u32, report: &mut LegacyMigration) {
    if depth > MAX_LEGACY_COPY_DEPTH {
        report.warnings.push(format!(
            "{} is nested deeper than {MAX_LEGACY_COPY_DEPTH} levels; not copied",
            old.display()
        ));
        return;
    }
    // A symlinked source directory is not followed: it could point anywhere,
    // and copying through it is not something a migration should decide.
    match fs::symlink_metadata(old) {
        Ok(metadata) if metadata.is_dir() => {}
        Ok(_) => return,
        Err(error) if error.kind() == io::ErrorKind::NotFound => return,
        Err(error) => {
            report
                .warnings
                .push(format!("cannot inspect {}: {error}", old.display()));
            return;
        }
    }
    let entries = match fs::read_dir(old) {
        Ok(entries) => entries,
        Err(error) => {
            report
                .warnings
                .push(format!("cannot read {}: {error}", old.display()));
            return;
        }
    };
    for entry in entries.flatten() {
        let name = entry.file_name();
        let old_child = old.join(&name);
        let new_child = new.join(&name);
        match fs::symlink_metadata(&old_child) {
            Ok(metadata) if metadata.is_dir() => {
                copy_legacy_tree(&old_child, &new_child, depth + 1, report)
            }
            // Lock files carry no data and their whole point is to be
            // per-installation.
            Ok(metadata)
                if metadata.is_file() && old_child.extension().is_some_and(|ext| ext == "lock") => {
            }
            Ok(metadata) if metadata.is_file() => copy_legacy_file(&old_child, &new_child, report),
            Ok(_) => {}
            Err(error) => report
                .warnings
                .push(format!("cannot inspect {}: {error}", old_child.display())),
        }
    }
}

/// Copy one legacy file if — and only if — the new path does not exist yet.
fn copy_legacy_file(old: &Path, new: &Path, report: &mut LegacyMigration) {
    // symlink_metadata, not exists(): a dangling symlink at the destination is
    // still something the user put there, and following it could write
    // somewhere unrelated.
    if fs::symlink_metadata(new).is_ok() {
        return;
    }
    match fs::symlink_metadata(old) {
        Ok(metadata) if metadata.is_file() => {}
        Ok(_) => {
            report.warnings.push(format!(
                "{} is not a regular file; not copied",
                old.display()
            ));
            return;
        }
        Err(error) if error.kind() == io::ErrorKind::NotFound => return,
        Err(error) => {
            report
                .warnings
                .push(format!("cannot inspect {}: {error}", old.display()));
            return;
        }
    }
    match copy_into_new_path(old, new) {
        Ok(true) => report
            .migrated
            .push(format!("{} -> {}", old.display(), new.display())),
        // The destination appeared while we were copying: whoever wrote it
        // wins, by design.
        Ok(false) => {}
        Err(error) => report.warnings.push(format!(
            "could not copy {} to {}: {error}",
            old.display(),
            new.display()
        )),
    }
}

/// Copy `old` to a sibling temporary file, then publish it with link(2), which
/// fails instead of overwriting an existing destination. Returns false when the
/// destination already existed.
fn copy_into_new_path(old: &Path, new: &Path) -> io::Result<bool> {
    let parent = new
        .parent()
        .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidInput, "destination has no parent"))?;
    ensure_private_dir(parent)?;

    let mut file_name = new.file_name().unwrap_or_default().to_os_string();
    file_name.push(format!(".jsh-migrate.{}", std::process::id()));
    let tmp = parent.join(file_name);
    let _ = fs::remove_file(&tmp); // A previous crashed pass may have left one.

    let result = (|| -> io::Result<bool> {
        let mut source = File::open(old)?;
        // 0600 from creation: session snapshots and the journal contain
        // environment variables, and history contains commands.
        let mut destination = OpenOptions::new()
            .write(true)
            .create_new(true)
            .mode(0o600)
            .open(&tmp)?;
        io::copy(&mut source, &mut destination)?;
        destination.sync_all()?;
        drop(destination);
        match fs::hard_link(&tmp, new) {
            Ok(()) => Ok(true),
            Err(error) if error.kind() == io::ErrorKind::AlreadyExists => Ok(false),
            Err(error) => Err(error),
        }
    })();
    let _ = fs::remove_file(&tmp);
    result
}

/// Create a destination directory the migration needs, private like the code
/// that normally owns it (`~/.jsh/sessions`, `~/.local/state/jsh`). An existing
/// directory keeps its permissions.
fn ensure_private_dir(dir: &Path) -> io::Result<()> {
    if fs::symlink_metadata(dir).is_ok() {
        return Ok(());
    }
    // Create every missing component 0700 instead of letting create_dir_all
    // leave the intermediates at the process umask: ~/.jsh holds session
    // snapshots, and those carry the session's environment variables.
    let mut missing = Vec::new();
    let mut cursor = Some(dir);
    while let Some(path) = cursor {
        if path.as_os_str().is_empty() || fs::symlink_metadata(path).is_ok() {
            break;
        }
        missing.push(path);
        cursor = path.parent();
    }
    for path in missing.into_iter().rev() {
        match fs::create_dir(path) {
            Ok(()) => fs::set_permissions(path, fs::Permissions::from_mode(0o700))?,
            Err(error) if error.kind() == io::ErrorKind::AlreadyExists => {}
            Err(error) => return Err(error),
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::environment::ShellState;
    use std::os::unix::fs::MetadataExt;

    #[test]
    fn test_parse_bash_output_env_vars() {
        let output = r#"=== ENV_VARS ===
TEST_VAR='hello_world'
MY_PATH='/custom/path'
=== ALIASES ===
=== FUNCTIONS ===
=== SHOPTS ==="#;

        let mut state = ShellState::new(false);
        parse_bash_output(output, &mut state);

        assert_eq!(state.get_var("TEST_VAR"), Some("hello_world"));
        assert_eq!(state.get_var("MY_PATH"), Some("/custom/path"));
    }

    #[test]
    fn test_parse_bash_output_aliases() {
        let output = r#"=== ENV_VARS ===
=== ALIASES ===
alias ll='ls -la'
alias grep='grep --color=auto'
=== FUNCTIONS ===
=== SHOPTS ==="#;

        let mut state = ShellState::new(false);
        parse_bash_output(output, &mut state);

        assert_eq!(state.aliases.get("ll"), Some(&"ls -la".to_string()));
        assert_eq!(
            state.aliases.get("grep"),
            Some(&"grep --color=auto".to_string())
        );
    }

    #[test]
    fn test_parse_bash_output_shopts() {
        let output = r#"=== ENV_VARS ===
=== ALIASES ===
=== FUNCTIONS ===
=== SHOPTS ===
extglob         on
dotglob         off
globstar        on"#;

        let mut state = ShellState::new(false);
        parse_bash_output(output, &mut state);

        assert!(state.shell_opts.extglob);
        assert!(!state.shell_opts.dotglob);
        assert!(state.shell_opts.globstar);
    }

    #[test]
    fn test_parse_bash_output_mixed() {
        let output = r#"=== ENV_VARS ===
APP_NAME='myapp'
=== ALIASES ===
alias ll='ls -lah'
=== FUNCTIONS ===
=== SHOPTS ===
extglob         on"#;

        let mut state = ShellState::new(false);
        parse_bash_output(output, &mut state);

        assert_eq!(state.get_var("APP_NAME"), Some("myapp"));
        assert_eq!(state.aliases.get("ll"), Some(&"ls -lah".to_string()));
        assert!(state.shell_opts.extglob);
    }

    #[test]
    fn bash_bridge_treats_special_config_path_as_data() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("startup \"$(false)\" file");
        std::fs::write(&path, "export JSH_SPECIAL_RC_PATH=loaded\n").expect("write rc file");

        let mut state = ShellState::new(false);
        source_via_bash(&path, &mut state);

        assert_eq!(state.get_var("JSH_SPECIAL_RC_PATH"), Some("loaded"));
        state.unset_var("JSH_SPECIAL_RC_PATH");
    }

    #[test]
    fn native_config_uses_program_control_flow() {
        let dir = tempfile::tempdir().expect("tempdir");
        let errexit_path = dir.path().join("errexit.jsh");
        std::fs::write(
            &errexit_path,
            "set -e; false; export JSH_AFTER_FAILED_RC=bad\n",
        )
        .expect("write rc file");

        let mut state = ShellState::new(false);
        load_config_file(&errexit_path, &mut state);
        assert_eq!(state.last_exit_code, 1);
        assert_eq!(state.get_var("JSH_AFTER_FAILED_RC"), None);

        let exit_path = dir.path().join("exit.jsh");
        std::fs::write(&exit_path, "exit 7; export JSH_AFTER_EXIT_RC=bad\n")
            .expect("write rc file");
        crate::builtins::reset_exit_request();
        load_config_file(&exit_path, &mut state);
        assert_eq!(state.last_exit_code, 7);
        assert_eq!(state.get_var("JSH_AFTER_EXIT_RC"), None);
        assert!(crate::builtins::EXIT_REQUESTED.load(std::sync::atomic::Ordering::SeqCst));

        let integration = parser::parse("export JSH_AFTER_PREEXISTING_EXIT=bad")
            .expect("parse integration command");
        executor::execute_program(&integration, &mut state);
        assert_eq!(state.last_exit_code, 7);
        assert_eq!(state.get_var("JSH_AFTER_PREEXISTING_EXIT"), None);
        crate::builtins::reset_exit_request();
    }

    // ── Legacy rsh → jsh migration ──────────────────────────────────────────
    //
    // Every test below drives `migrate_legacy_rsh_data_in` with an explicit
    // root under the test's own tempdir. The implicit `migrate_legacy_rsh_data`
    // entry point resolves $HOME and is disabled under cfg(test) precisely so
    // that no unit test can write into the developer's home directory.

    const OLD_HISTORY: &str = concat!(
        r#"{"rsh_history_version":1,"command":"echo one","timestamp":10,"cwd":"/p"}"#,
        "\n",
        r#"{"rsh_history_version":1,"command":"make -j","timestamp":11,"cwd":"/p"}"#,
        "\n"
    );

    fn legacy_layout(root: &Path) -> (PathBuf, PathBuf) {
        let home = root.join("home");
        let state = root.join("state");
        fs::create_dir_all(home.join(".rsh").join("sessions")).expect("legacy sessions dir");
        fs::create_dir_all(home.join(".rsh").join("workflows")).expect("legacy workflows dir");
        fs::create_dir_all(state.join("rsh")).expect("legacy state dir");
        fs::write(home.join(".rshrc"), "export FROM_RSHRC=1\n").expect("rc");
        fs::write(home.join(".rsh_history"), OLD_HISTORY).expect("history");
        fs::write(home.join(".rsh_bookmarks"), "proj|/p\n").expect("bookmarks");
        fs::write(home.join(".rsh_z"), "/p|1.5|10\n").expect("zjump");
        fs::write(home.join(".rsh").join("sessions").join("tab1.json"), "{}").expect("snapshot");
        fs::write(
            home.join(".rsh").join("workflows").join("w.yaml"),
            "name: w",
        )
        .expect("workflow");
        fs::write(state.join("rsh").join("executions.jsonl"), "{}\n").expect("journal");
        fs::write(state.join("rsh").join("executions.lock"), "").expect("journal lock");
        (home, state)
    }

    fn mode_of(path: &Path) -> u32 {
        fs::metadata(path).expect("metadata").permissions().mode() & 0o777
    }

    #[test]
    fn legacy_rsh_data_is_copied_when_the_jsh_path_is_absent() {
        let temp = tempfile::tempdir().expect("tempdir");
        let (home, state) = legacy_layout(temp.path());

        let report = migrate_legacy_rsh_data_in(&home, &state);

        assert!(report.warnings.is_empty(), "{:?}", report.warnings);
        assert_eq!(report.migrated.len(), 7, "{:?}", report.migrated);
        assert_eq!(
            fs::read_to_string(home.join(".jsh_history")).expect("new history"),
            OLD_HISTORY
        );
        assert_eq!(
            fs::read_to_string(home.join(".jshrc")).expect("new rc"),
            "export FROM_RSHRC=1\n"
        );
        assert_eq!(
            fs::read_to_string(home.join(".jsh_bookmarks")).expect("new bookmarks"),
            "proj|/p\n"
        );
        assert_eq!(
            fs::read_to_string(home.join(".jsh_z")).expect("new zjump db"),
            "/p|1.5|10\n"
        );
        assert!(home
            .join(".jsh")
            .join("sessions")
            .join("tab1.json")
            .is_file());
        assert!(home.join(".jsh").join("workflows").join("w.yaml").is_file());
        assert!(state.join("jsh").join("executions.jsonl").is_file());
        // Lock files are per-installation coordination, not data.
        assert!(!state.join("jsh").join("executions.lock").exists());

        // Copy, never move: an installed rsh binary may still be in use.
        assert!(home.join(".rsh_history").is_file());
        assert!(home.join(".rshrc").is_file());
        assert!(home
            .join(".rsh")
            .join("sessions")
            .join("tab1.json")
            .is_file());
        assert!(state.join("rsh").join("executions.jsonl").is_file());

        // History, snapshots and the journal are private data.
        assert_eq!(mode_of(&home.join(".jsh_history")), 0o600);
        assert_eq!(
            mode_of(&home.join(".jsh").join("sessions").join("tab1.json")),
            0o600
        );
        assert_eq!(mode_of(&home.join(".jsh").join("sessions")), 0o700);
        assert_eq!(mode_of(&home.join(".jsh")), 0o700);
        assert_eq!(mode_of(&state.join("jsh")), 0o700);

        // No temporary artefacts left behind.
        let leftovers: Vec<String> = fs::read_dir(&home)
            .expect("read home")
            .flatten()
            .map(|entry| entry.file_name().to_string_lossy().into_owned())
            .filter(|name| name.contains("jsh-migrate"))
            .collect();
        assert!(leftovers.is_empty(), "{leftovers:?}");
    }

    #[test]
    fn migration_never_overwrites_existing_jsh_data() {
        let temp = tempfile::tempdir().expect("tempdir");
        let (home, state) = legacy_layout(temp.path());
        fs::write(home.join(".jsh_history"), "keep me\n").expect("existing history");
        fs::create_dir_all(home.join(".jsh").join("sessions")).expect("existing sessions dir");
        fs::write(
            home.join(".jsh").join("sessions").join("tab1.json"),
            "{\"keep\":1}",
        )
        .expect("existing snapshot");

        let report = migrate_legacy_rsh_data_in(&home, &state);

        assert_eq!(
            fs::read_to_string(home.join(".jsh_history")).expect("history"),
            "keep me\n"
        );
        assert_eq!(
            fs::read_to_string(home.join(".jsh").join("sessions").join("tab1.json"))
                .expect("snapshot"),
            "{\"keep\":1}"
        );
        assert!(!report
            .migrated
            .iter()
            .any(|line| line.contains(".rsh_history")));
        // Untouched destinations do not stop the rest of the pass.
        assert!(report.migrated.iter().any(|line| line.contains(".rshrc")));
    }

    #[test]
    fn a_second_migration_pass_is_a_no_op() {
        let temp = tempfile::tempdir().expect("tempdir");
        let (home, state) = legacy_layout(temp.path());

        let first = migrate_legacy_rsh_data_in(&home, &state);
        assert!(!first.migrated.is_empty());
        let history_before = fs::metadata(home.join(".jsh_history")).expect("history metadata");

        let second = migrate_legacy_rsh_data_in(&home, &state);
        assert!(second.is_empty(), "{second:?}");
        // Same inode: the file was not rewritten, not even identically.
        assert_eq!(
            history_before.ino(),
            fs::metadata(home.join(".jsh_history"))
                .expect("history metadata")
                .ino()
        );
    }

    #[test]
    fn an_unreadable_legacy_file_is_a_warning_and_the_rest_still_migrates() {
        let temp = tempfile::tempdir().expect("tempdir");
        let (home, state) = legacy_layout(temp.path());
        fs::set_permissions(home.join(".rsh_history"), fs::Permissions::from_mode(0o000))
            .expect("make history unreadable");
        fs::set_permissions(
            home.join(".rsh").join("workflows"),
            fs::Permissions::from_mode(0o000),
        )
        .expect("make workflows unreadable");

        let report = migrate_legacy_rsh_data_in(&home, &state);

        assert!(
            report
                .warnings
                .iter()
                .any(|warning| warning.contains(".rsh_history")),
            "{:?}",
            report.warnings
        );
        assert!(
            report
                .warnings
                .iter()
                .any(|warning| warning.contains("workflows")),
            "{:?}",
            report.warnings
        );
        assert!(!home.join(".jsh_history").exists());
        assert!(home.join(".jshrc").is_file());
        assert!(state.join("jsh").join("executions.jsonl").is_file());

        // Leave the tempdir removable.
        fs::set_permissions(
            home.join(".rsh").join("workflows"),
            fs::Permissions::from_mode(0o700),
        )
        .expect("restore workflows permissions");
    }

    #[test]
    fn a_symlink_is_never_followed_on_either_side() {
        let temp = tempfile::tempdir().expect("tempdir");
        let (home, state) = legacy_layout(temp.path());
        let outside = temp.path().join("outside.txt");
        fs::write(&outside, "must not be touched\n").expect("outside file");

        // A symlinked source is data we refuse to interpret.
        fs::remove_file(home.join(".rsh_bookmarks")).expect("drop bookmarks");
        std::os::unix::fs::symlink(&outside, home.join(".rsh_bookmarks")).expect("link source");
        // A dangling destination symlink is still something the user placed.
        std::os::unix::fs::symlink(temp.path().join("nowhere"), home.join(".jsh_z"))
            .expect("link destination");

        let report = migrate_legacy_rsh_data_in(&home, &state);

        assert!(!home.join(".jsh_bookmarks").exists());
        assert!(report
            .warnings
            .iter()
            .any(|warning| warning.contains(".rsh_bookmarks")));
        assert!(!temp.path().join("nowhere").exists());
        assert_eq!(
            fs::read_to_string(&outside).expect("outside file"),
            "must not be touched\n"
        );
    }
}
