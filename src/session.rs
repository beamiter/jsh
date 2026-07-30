/// Session persistence: save/restore shell state across terminal restarts.
///
/// When jterm4 spawns jsh with `--session <id>`, jsh restores state from
/// `~/.jsh/sessions/<id>.json`. On exit, jsh saves a snapshot back.
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::fs::{self, File, OpenOptions};
use std::io::{self, Write};
use std::os::unix::fs::{OpenOptionsExt, PermissionsExt};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};

use crate::environment::*;
use crate::parser::ast::CompoundCommand;

/// Snapshot format version. Bump when adding fields (use #[serde(default)] for compat).
const SNAPSHOT_VERSION: u32 = 1;
static TEMP_FILE_COUNTER: AtomicU64 = AtomicU64::new(0);

/// Environment variables that should NOT be persisted across sessions.
/// These are process-specific or terminal-specific and would be stale after restore.
const SKIP_ENV_VARS: &[&str] = &[
    // Process-specific
    "BASHPID",
    "PPID",
    "SHLVL",
    "_",
    "OLDPWD",
    // Terminal-specific (re-set by the new terminal)
    "COLUMNS",
    "LINES",
    "TERM",
    "COLORTERM",
    // Same criterion as TERM/COLORTERM, and the omission mattered: restoring a
    // session captured under one terminal into another left the first
    // terminal's identity in the live shell, so anything branching on
    // TERM_PROGRAM saw the wrong emulator.
    "TERM_PROGRAM",
    "TERM_PROGRAM_VERSION",
    "VTE_VERSION",
    "WINDOWID",
    "DISPLAY",
    "WAYLAND_DISPLAY",
    // Session-specific
    "SSH_AUTH_SOCK",
    "SSH_AGENT_PID",
    "SSH_CONNECTION",
    "SSH_CLIENT",
    "SSH_TTY",
    "GPG_AGENT_INFO",
    "DBUS_SESSION_BUS_ADDRESS",
    "XDG_SESSION_ID",
    "XDG_RUNTIME_DIR",
    // Internal
    "JSH_SESSION_ID",
    "TERM_SESSION_ID",
];

/// Environment variable names that are likely to hold credentials. Session
/// snapshots are a convenience cache, not a secret store, so these values must
/// come from the newly launched process instead of being persisted to disk.
fn is_likely_secret_env(name: &str) -> bool {
    let upper = name.to_ascii_uppercase();
    let parts: Vec<&str> = upper
        .split(|c: char| !c.is_ascii_alphanumeric())
        .filter(|part| !part.is_empty())
        .collect();

    if parts.iter().any(|part| {
        matches!(
            *part,
            "TOKEN"
                | "SECRET"
                | "PASSWORD"
                | "PASSWD"
                | "CREDENTIAL"
                | "CREDENTIALS"
                | "COOKIE"
                | "AUTH"
                | "AUTHORIZATION"
                | "PAT"
                | "DSN"
        )
    }) {
        return true;
    }

    upper.ends_with("PASSWORD")
        || upper.ends_with("_PWD")
        || upper == "KEY"
        || upper.ends_with("_KEY")
        || upper.contains("_KEY_")
        || matches!(
            upper.as_str(),
            "APIKEY" | "ACCESSKEY" | "SECRETKEY" | "PRIVATEKEY" | "DATABASE_URL"
        )
        || upper.ends_with("_DATABASE_URL")
}

fn should_persist_env(name: &str) -> bool {
    !SKIP_ENV_VARS.contains(&name) && !is_likely_secret_env(name)
}

/// Detected environment context for re-activation on restore.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub enum EnvironmentContext {
    Plain,
    PythonVenv {
        virtual_env: String,
    },
    NixShell {
        #[serde(default)]
        flake_dir: Option<String>,
        nix_build_top: Option<String>,
    },
    Docker {
        container_id: Option<String>,
    },
    Ssh {
        ssh_connection: String,
    },
}

impl Default for EnvironmentContext {
    fn default() -> Self {
        EnvironmentContext::Plain
    }
}

/// Serializable snapshot of shell session state.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SessionSnapshot {
    pub version: u32,
    pub session_id: String,
    pub cwd: String,
    pub env_vars: HashMap<String, String>,
    pub aliases: HashMap<String, String>,
    pub functions: HashMap<String, CompoundCommand>,
    pub arrays: HashMap<String, Vec<String>>,
    pub assoc_arrays: HashMap<String, HashMap<String, String>>,
    pub shell_opts: ShellOpts,
    pub hooks: ShellHooks,
    pub traps: HashMap<String, String>,
    pub completion_specs: HashMap<String, CompletionSpec>,
    pub dir_stack: Vec<PathBuf>,
    pub editing_mode: EditingMode,
    pub prompt_style: PromptStyle,
    pub last_exit_code: i32,
    pub notification_threshold_secs: u64,
    #[serde(default)]
    pub environment_context: EnvironmentContext,
}

/// Directory where session snapshot files are stored.
///
/// Every default-location save/load/list funnels through here, which makes it
/// the one place that has to copy pre-rename ~/.rsh/sessions snapshots across.
fn sessions_dir() -> PathBuf {
    crate::config::migrate_legacy_rsh_data();
    dirs::home_dir()
        .unwrap_or_else(|| PathBuf::from("/tmp"))
        .join(".jsh")
        .join("sessions")
}

/// Full path for a session snapshot file.
fn session_file(session_id: &str) -> PathBuf {
    sessions_dir().join(format!("{}.json", sanitize_session_id(session_id)))
}

fn sanitize_session_id(session_id: &str) -> String {
    session_id
        .chars()
        .filter(|c| c.is_alphanumeric() || *c == '-' || *c == '_')
        .collect()
}

fn session_file_in(dir: &Path, session_id: &str) -> io::Result<PathBuf> {
    let safe_id = sanitize_session_id(session_id);
    if safe_id.is_empty() || safe_id != session_id {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "session ID may contain only letters, digits, '-' and '_'",
        ));
    }
    Ok(dir.join(format!("{}.json", safe_id)))
}

fn ensure_private_directory(dir: &Path) -> io::Result<()> {
    if let Ok(metadata) = fs::symlink_metadata(dir) {
        if metadata.file_type().is_symlink() {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "session directory must not be a symlink",
            ));
        }
    }
    fs::create_dir_all(dir)?;
    let metadata = fs::symlink_metadata(dir)?;
    if !metadata.is_dir() {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "session path is not a directory",
        ));
    }
    fs::set_permissions(dir, fs::Permissions::from_mode(0o700))
}

fn ensure_private_file(path: &Path) -> io::Result<()> {
    let metadata = fs::symlink_metadata(path)?;
    if metadata.file_type().is_symlink() || !metadata.is_file() {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "session snapshot must be a regular file",
        ));
    }
    fs::set_permissions(path, fs::Permissions::from_mode(0o600))
}

impl SessionSnapshot {
    /// Capture the current shell state into a serializable snapshot.
    pub fn capture(state: &ShellState, session_id: &str) -> Self {
        let cwd = std::env::current_dir()
            .map(|p| p.to_string_lossy().to_string())
            .unwrap_or_else(|_| "/".to_string());

        // Filter out process/terminal-specific env vars
        let env_vars: HashMap<String, String> = state
            .env_vars
            .iter()
            .filter(|(k, _)| should_persist_env(k))
            .map(|(k, v)| (k.clone(), v.clone()))
            .collect();

        SessionSnapshot {
            version: SNAPSHOT_VERSION,
            session_id: session_id.to_string(),
            cwd,
            env_vars,
            aliases: state.aliases.clone(),
            functions: state.functions.clone(),
            arrays: state.arrays.clone(),
            assoc_arrays: state.assoc_arrays.clone(),
            shell_opts: state.shell_opts.clone(),
            hooks: state.hooks.clone(),
            traps: state.traps.clone(),
            completion_specs: state.completion_specs.clone(),
            dir_stack: state.dir_stack.clone(),
            editing_mode: state.editing_mode.clone(),
            prompt_style: state.prompt_style,
            last_exit_code: state.last_exit_code,
            notification_threshold_secs: state.notification_threshold.as_secs(),
            environment_context: detect_environment(),
        }
    }

    /// Apply this snapshot to a ShellState, restoring its fields.
    pub fn apply(self, state: &mut ShellState) {
        // Restore CWD
        if let Err(e) = std::env::set_current_dir(&self.cwd) {
            eprintln!("jsh: session restore: failed to cd to {}: {}", self.cwd, e);
        }

        // Merge env vars: snapshot values override, but keep process-inherited vars for SKIP list
        for (k, v) in &self.env_vars {
            if should_persist_env(k) {
                state.env_vars.insert(k.clone(), v.clone());
                std::env::set_var(k, v);
            }
        }

        state.aliases = self.aliases;
        state.functions = self.functions;
        state.arrays = self.arrays;
        state.assoc_arrays = self.assoc_arrays;
        state.shell_opts = self.shell_opts;
        state.hooks = self.hooks;
        state.traps = self.traps;
        state.completion_specs = self.completion_specs;
        state.dir_stack = self.dir_stack;
        state.editing_mode = self.editing_mode;
        state.prompt_style = self.prompt_style;
        state.last_exit_code = self.last_exit_code;
        state.notification_threshold =
            std::time::Duration::from_secs(self.notification_threshold_secs);
    }

    /// Save snapshot to disk as JSON (atomic write).
    pub fn save(&self) -> Result<(), std::io::Error> {
        self.save_to_dir(&sessions_dir())
    }

    fn save_to_dir(&self, dir: &Path) -> io::Result<()> {
        if self.version != SNAPSHOT_VERSION {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                format!(
                    "cannot save unsupported session snapshot version {} (supported: {})",
                    self.version, SNAPSHOT_VERSION
                ),
            ));
        }
        ensure_private_directory(dir)?;

        let path = session_file_in(dir, &self.session_id)?;
        let counter = TEMP_FILE_COUNTER.fetch_add(1, Ordering::Relaxed);
        let timestamp = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|duration| duration.as_nanos())
            .unwrap_or(0);
        let tmp_path = path.with_extension(format!(
            "json.tmp.{}.{}.{}",
            std::process::id(),
            timestamp,
            counter
        ));

        // Keep the write boundary defensive too: SessionSnapshot fields are
        // public, so callers can construct or mutate one without `capture`.
        let mut persisted = self.clone();
        persisted
            .env_vars
            .retain(|name, _| should_persist_env(name));
        let json = serde_json::to_string_pretty(&persisted).map_err(io::Error::other)?;

        let result = (|| {
            let mut file = OpenOptions::new()
                .write(true)
                .create_new(true)
                .mode(0o600)
                .open(&tmp_path)?;
            fs::set_permissions(&tmp_path, fs::Permissions::from_mode(0o600))?;
            file.write_all(json.as_bytes())?;
            file.sync_all()?;

            // On Unix, rename replaces an existing regular file atomically.
            // Do not unlink the old snapshot first: doing so creates a window
            // where a crash would leave no recoverable state.
            fs::rename(&tmp_path, &path)?;
            ensure_private_file(&path)?;
            if let Ok(directory) = File::open(dir) {
                let _ = directory.sync_all();
            }
            Ok(())
        })();
        if result.is_err() {
            let _ = fs::remove_file(&tmp_path);
        }
        result
    }

    /// Load a snapshot from disk by session ID.
    pub fn load(session_id: &str) -> Result<Self, Box<dyn std::error::Error>> {
        Self::load_from_dir(session_id, &sessions_dir())
    }

    fn load_from_dir(session_id: &str, dir: &Path) -> Result<Self, Box<dyn std::error::Error>> {
        if dir.exists() {
            ensure_private_directory(dir)?;
        }
        let path = session_file_in(dir, session_id)?;
        ensure_private_file(&path)?;
        let json = fs::read_to_string(&path)?;
        let mut snapshot: SessionSnapshot = serde_json::from_str(&json)?;
        if snapshot.version != SNAPSHOT_VERSION {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                format!(
                    "unsupported session snapshot version {} (supported: {})",
                    snapshot.version, SNAPSHOT_VERSION
                ),
            )
            .into());
        }
        // Old version-1 snapshots may predate secret filtering. Never return
        // those stale credentials to the restore path.
        snapshot.env_vars.retain(|name, _| should_persist_env(name));
        Ok(snapshot)
    }

    /// Explicitly delete a session snapshot.
    pub fn delete(session_id: &str) {
        delete_from_dir(&sessions_dir(), session_id);
    }
}

fn delete_from_dir(dir: &Path, session_id: &str) {
    if !dir.exists() || ensure_private_directory(dir).is_err() {
        return;
    }
    let Ok(path) = session_file_in(dir, session_id) else {
        return;
    };
    if ensure_private_file(&path).is_ok() {
        let _ = fs::remove_file(path);
    }
}

/// Search for `flake.nix` starting from CWD and walking up to parent directories.
/// Returns the directory containing the flake, or None.
fn find_flake_dir() -> Option<String> {
    let mut dir = std::env::current_dir().ok()?;
    loop {
        if dir.join("flake.nix").exists() {
            return Some(dir.to_string_lossy().to_string());
        }
        if !dir.pop() {
            return None;
        }
    }
}

/// Detect the current environment context by checking env vars and filesystem markers.
pub fn detect_environment() -> EnvironmentContext {
    // Python venv
    if let Ok(venv) = std::env::var("VIRTUAL_ENV") {
        if !venv.is_empty() {
            return EnvironmentContext::PythonVenv { virtual_env: venv };
        }
    }

    // Nix shell — check env var first (jsh is the nix shell itself),
    // then check for flake.nix (jsh is the parent, nix develop ran as child)
    let in_nix = std::env::var("IN_NIX_SHELL").is_ok() || std::env::var("NIX_BUILD_TOP").is_ok();
    let flake_dir = find_flake_dir();
    if in_nix || flake_dir.is_some() {
        return EnvironmentContext::NixShell {
            flake_dir,
            nix_build_top: std::env::var("NIX_BUILD_TOP").ok(),
        };
    }

    // Docker
    if std::path::Path::new("/.dockerenv").exists() || std::env::var("DOCKER_CONTAINER").is_ok() {
        let container_id = std::fs::read_to_string("/etc/hostname")
            .ok()
            .map(|s| s.trim().to_string());
        return EnvironmentContext::Docker { container_id };
    }

    // SSH
    if let Ok(conn) = std::env::var("SSH_CONNECTION") {
        if !conn.is_empty() {
            return EnvironmentContext::Ssh {
                ssh_connection: conn,
            };
        }
    }
    if std::env::var("SSH_CLIENT").is_ok() {
        let conn = std::env::var("SSH_CLIENT").unwrap_or_default();
        return EnvironmentContext::Ssh {
            ssh_connection: conn,
        };
    }

    EnvironmentContext::Plain
}

/// Re-activate environment context after restoring a session.
pub fn reactivate_environment(ctx: &EnvironmentContext, state: &mut ShellState) {
    match ctx {
        EnvironmentContext::PythonVenv { virtual_env } => {
            let venv_path = std::path::Path::new(virtual_env);
            let activate = venv_path.join("bin").join("activate");
            if activate.exists() {
                // Set VIRTUAL_ENV and prepend its bin to PATH
                state.export_var("VIRTUAL_ENV", virtual_env);
                let venv_bin = venv_path.join("bin");
                if let Some(path) = state.env_vars.get("PATH").cloned() {
                    let venv_bin_str = venv_bin.to_string_lossy();
                    // Only prepend if not already there
                    if !path.split(':').any(|p| p == venv_bin_str.as_ref()) {
                        let new_path = format!("{}:{}", venv_bin_str, path);
                        state.export_var("PATH", &new_path);
                    }
                }
            } else {
                eprintln!(
                    "jsh: session restore: venv {} no longer exists",
                    virtual_env
                );
            }
        }
        EnvironmentContext::NixShell { .. } => {
            // Do not auto-restore the nix develop environment on session
            // restore — let the user re-enter `nix develop` explicitly.
        }
        EnvironmentContext::Docker { .. } | EnvironmentContext::Ssh { .. } => {
            // Docker/SSH context is informational at the jsh level.
            // Re-establishing the connection is jterm4's responsibility.
        }
        EnvironmentContext::Plain => {}
    }
}

/// Clean up session files older than max_age.
pub fn cleanup_stale_sessions(max_age: std::time::Duration) {
    let dir = sessions_dir();
    cleanup_stale_sessions_in(&dir, max_age);
}

fn cleanup_stale_sessions_in(dir: &Path, max_age: std::time::Duration) {
    // Cleanup must enforce the same trust boundary as save/load. In particular,
    // never traverse a sessions-directory symlink or follow symlinked entries.
    if !dir.exists() || ensure_private_directory(dir).is_err() {
        return;
    }
    let entries = match std::fs::read_dir(dir) {
        Ok(e) => e,
        Err(_) => return,
    };

    for entry in entries.flatten() {
        if !entry.file_type().is_ok_and(|file_type| file_type.is_file()) {
            continue;
        }
        let path = entry.path();
        if path.extension().and_then(|e| e.to_str()) != Some("json") {
            continue;
        }
        if let Ok(metadata) = path.metadata() {
            if let Ok(modified) = metadata.modified() {
                if let Ok(age) = modified.elapsed() {
                    if age > max_age {
                        let _ = std::fs::remove_file(&path);
                    }
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::os::unix::fs::PermissionsExt;

    /// A snapshot is portable between terminals, so every variable naming the
    /// terminal that captured it has to be left behind on restore.
    #[test]
    fn test_terminal_identity_is_not_persisted() {
        for name in [
            "TERM",
            "COLORTERM",
            "TERM_PROGRAM",
            "TERM_PROGRAM_VERSION",
            "VTE_VERSION",
        ] {
            assert!(
                !should_persist_env(name),
                "{name} would survive a restore into a different terminal"
            );
        }
        assert!(should_persist_env("EDITOR"));
    }

    #[test]
    fn test_session_snapshot_roundtrip() {
        let mut state = ShellState::new(false);
        state.aliases.insert("ll".to_string(), "ls -la".to_string());
        state.export_var("MY_VAR", "hello");
        state.shell_opts.extglob = true;
        state.hooks.precmd.push("echo hi".to_string());
        state
            .traps
            .insert("EXIT".to_string(), "echo bye".to_string());

        let snapshot = SessionSnapshot::capture(&state, "test-roundtrip");
        let json = serde_json::to_string_pretty(&snapshot).expect("serialize");
        let restored: SessionSnapshot = serde_json::from_str(&json).expect("deserialize");

        assert_eq!(restored.session_id, "test-roundtrip");
        assert_eq!(restored.aliases.get("ll"), Some(&"ls -la".to_string()));
        assert_eq!(restored.env_vars.get("MY_VAR"), Some(&"hello".to_string()));
        assert_eq!(restored.shell_opts.extglob, true);
        assert_eq!(restored.hooks.precmd, vec!["echo hi".to_string()]);
        assert_eq!(restored.traps.get("EXIT"), Some(&"echo bye".to_string()));
    }

    #[test]
    fn test_env_var_filtering() {
        let mut state = ShellState::new(false);
        state
            .env_vars
            .insert("COLUMNS".to_string(), "80".to_string());
        state.env_vars.insert("LINES".to_string(), "24".to_string());
        state
            .env_vars
            .insert("MY_APP".to_string(), "value".to_string());
        state
            .env_vars
            .insert("OPENAI_API_KEY".to_string(), "sk-secret".to_string());
        state
            .env_vars
            .insert("GITHUB_TOKEN".to_string(), "token".to_string());
        state
            .env_vars
            .insert("DB_PASSWORD".to_string(), "password".to_string());
        state.env_vars.insert(
            "AWS_SECRET_ACCESS_KEY".to_string(),
            "aws-secret".to_string(),
        );
        state
            .env_vars
            .insert("GITHUB_PAT".to_string(), "github-pat".to_string());
        state
            .env_vars
            .insert("SIGNING_PRIVATE_KEY".to_string(), "private-key".to_string());
        state.env_vars.insert(
            "DATABASE_URL".to_string(),
            "postgres://user:pass@host/db".to_string(),
        );
        state.env_vars.insert(
            "SENTRY_DSN".to_string(),
            "https://secret@example.invalid/1".to_string(),
        );
        state
            .env_vars
            .insert("PGPASSWORD".to_string(), "postgres-secret".to_string());
        state
            .env_vars
            .insert("MYSQL_PWD".to_string(), "mysql-secret".to_string());
        state.env_vars.insert(
            "NPM_CONFIG__AUTH".to_string(),
            "npm-auth-secret".to_string(),
        );
        state.env_vars.insert(
            "DOCKER_AUTH_CONFIG".to_string(),
            "docker-auth-secret".to_string(),
        );
        state.env_vars.insert(
            "SSH_CONNECTION".to_string(),
            "203.0.113.1 12345 192.0.2.1 22".to_string(),
        );
        state
            .env_vars
            .insert("SSH_CLIENT".to_string(), "203.0.113.1 12345 22".to_string());
        state
            .env_vars
            .insert("SSH_TTY".to_string(), "/dev/pts/9".to_string());

        let snapshot = SessionSnapshot::capture(&state, "test-filter");
        assert!(!snapshot.env_vars.contains_key("COLUMNS"));
        assert!(!snapshot.env_vars.contains_key("LINES"));
        assert!(!snapshot.env_vars.contains_key("OPENAI_API_KEY"));
        assert!(!snapshot.env_vars.contains_key("GITHUB_TOKEN"));
        assert!(!snapshot.env_vars.contains_key("DB_PASSWORD"));
        assert!(!snapshot.env_vars.contains_key("AWS_SECRET_ACCESS_KEY"));
        assert!(!snapshot.env_vars.contains_key("GITHUB_PAT"));
        assert!(!snapshot.env_vars.contains_key("SIGNING_PRIVATE_KEY"));
        assert!(!snapshot.env_vars.contains_key("DATABASE_URL"));
        assert!(!snapshot.env_vars.contains_key("SENTRY_DSN"));
        assert!(!snapshot.env_vars.contains_key("PGPASSWORD"));
        assert!(!snapshot.env_vars.contains_key("MYSQL_PWD"));
        assert!(!snapshot.env_vars.contains_key("NPM_CONFIG__AUTH"));
        assert!(!snapshot.env_vars.contains_key("DOCKER_AUTH_CONFIG"));
        assert!(!snapshot.env_vars.contains_key("SSH_CONNECTION"));
        assert!(!snapshot.env_vars.contains_key("SSH_CLIENT"));
        assert!(!snapshot.env_vars.contains_key("SSH_TTY"));
        assert_eq!(snapshot.env_vars.get("MY_APP"), Some(&"value".to_string()));
    }

    #[test]
    fn test_session_file_path_sanitization() {
        let path = session_file("../../../etc/passwd");
        assert!(!path.to_string_lossy().contains(".."));
        assert!(path.to_string_lossy().contains("etcpasswd"));
    }

    #[test]
    fn session_save_is_atomic_private_and_repeatable() {
        let temp = tempfile::tempdir().expect("tempdir");
        let dir = temp.path().join("sessions");
        let mut state = ShellState::new(false);
        state.aliases.insert("ll".into(), "ls -la".into());
        let mut snapshot = SessionSnapshot::capture(&state, "private-session");
        snapshot
            .env_vars
            .insert("MANUALLY_ADDED_TOKEN".into(), "never-write-me".into());
        snapshot.save_to_dir(&dir).expect("first save");

        let dir_mode = fs::metadata(&dir)
            .expect("dir metadata")
            .permissions()
            .mode()
            & 0o777;
        assert_eq!(dir_mode, 0o700);
        let path = session_file_in(&dir, "private-session").expect("session path");
        let file_mode = fs::metadata(&path)
            .expect("snapshot metadata")
            .permissions()
            .mode()
            & 0o777;
        assert_eq!(file_mode, 0o600);

        // Re-saving atomically replaces the prior snapshot and leaves no temp
        // artifacts behind.
        snapshot.aliases.insert("gs".into(), "git status".into());
        snapshot.save_to_dir(&dir).expect("second save");
        let loaded = SessionSnapshot::load_from_dir("private-session", &dir).expect("load");
        assert_eq!(loaded.aliases.get("gs"), Some(&"git status".to_string()));
        assert!(!loaded.env_vars.contains_key("MANUALLY_ADDED_TOKEN"));
        assert!(!fs::read_to_string(&path)
            .expect("snapshot contents")
            .contains("never-write-me"));
        let names: Vec<String> = fs::read_dir(&dir)
            .expect("read sessions")
            .flatten()
            .map(|entry| entry.file_name().to_string_lossy().into_owned())
            .collect();
        assert_eq!(names, vec!["private-session.json"]);
    }

    /// Saved sessions moved from ~/.rsh/sessions to ~/.jsh/sessions with the
    /// 0.2.0 rename. A snapshot copied across must still load — including its
    /// 0600/0700 privacy requirements, which `load_from_dir` enforces.
    #[test]
    fn a_migrated_snapshot_loads_from_the_new_sessions_directory() {
        let temp = tempfile::tempdir().expect("tempdir");
        let home = temp.path().join("home");
        let state = temp.path().join("state");
        let legacy_dir = home.join(".rsh").join("sessions");
        ensure_private_directory(&legacy_dir).expect("legacy sessions dir");

        let mut snapshot_state = ShellState::new(false);
        snapshot_state.aliases.insert("ll".into(), "ls -la".into());
        let snapshot = SessionSnapshot::capture(&snapshot_state, "tab1");
        let json = serde_json::to_vec(&snapshot).expect("serialize");
        fs::write(legacy_dir.join("tab1.json"), &json).expect("legacy snapshot");

        let report = crate::config::migrate_legacy_rsh_data_in(&home, &state);
        assert!(report.warnings.is_empty(), "{report:?}");

        let new_dir = home.join(".jsh").join("sessions");
        let loaded = SessionSnapshot::load_from_dir("tab1", &new_dir).expect("load migrated");
        assert_eq!(loaded.session_id, "tab1");
        assert_eq!(loaded.aliases.get("ll"), Some(&"ls -la".to_string()));
        // The rsh copy stays where it was.
        assert!(legacy_dir.join("tab1.json").is_file());
    }

    #[test]
    fn load_rejects_unsupported_snapshot_versions() {
        let temp = tempfile::tempdir().expect("tempdir");
        let dir = temp.path().join("sessions");
        ensure_private_directory(&dir).expect("session dir");
        let state = ShellState::new(false);
        let mut snapshot = SessionSnapshot::capture(&state, "future");
        snapshot.version = SNAPSHOT_VERSION + 1;
        let path = session_file_in(&dir, "future").expect("session path");
        let json = serde_json::to_vec(&snapshot).expect("serialize fixture");
        let mut file = OpenOptions::new()
            .write(true)
            .create_new(true)
            .mode(0o600)
            .open(&path)
            .expect("write fixture");
        file.write_all(&json).expect("fixture contents");

        let error = SessionSnapshot::load_from_dir("future", &dir).expect_err("reject version");
        assert!(error
            .to_string()
            .contains("unsupported session snapshot version"));
    }

    #[test]
    fn loading_does_not_consume_snapshot() {
        let temp = tempfile::tempdir().expect("tempdir");
        let dir = temp.path().join("sessions");
        let state = ShellState::new(false);
        let snapshot = SessionSnapshot::capture(&state, "persistent");
        snapshot.save_to_dir(&dir).expect("save");

        SessionSnapshot::load_from_dir("persistent", &dir).expect("first load");
        SessionSnapshot::load_from_dir("persistent", &dir).expect("second load");
        assert!(session_file_in(&dir, "persistent")
            .expect("session path")
            .exists());
    }

    #[test]
    fn load_filters_secrets_from_existing_version_one_snapshot() {
        let temp = tempfile::tempdir().expect("tempdir");
        let dir = temp.path().join("sessions");
        ensure_private_directory(&dir).expect("session dir");
        let state = ShellState::new(false);
        let mut snapshot = SessionSnapshot::capture(&state, "legacy-secret");
        snapshot
            .env_vars
            .insert("LEGACY_TOKEN".into(), "stale-token".into());
        let path = session_file_in(&dir, "legacy-secret").expect("session path");
        let json = serde_json::to_vec(&snapshot).expect("serialize fixture");
        let mut file = OpenOptions::new()
            .write(true)
            .create_new(true)
            .mode(0o600)
            .open(&path)
            .expect("write fixture");
        file.write_all(&json).expect("fixture contents");

        let loaded = SessionSnapshot::load_from_dir("legacy-secret", &dir).expect("load");
        assert!(!loaded.env_vars.contains_key("LEGACY_TOKEN"));
    }

    #[test]
    fn invalid_session_id_is_rejected_for_io() {
        let temp = tempfile::tempdir().expect("tempdir");
        let state = ShellState::new(false);
        let snapshot = SessionSnapshot::capture(&state, "../../");
        let error = snapshot
            .save_to_dir(temp.path())
            .expect_err("unsafe session id");
        assert_eq!(error.kind(), io::ErrorKind::InvalidInput);

        let snapshot = SessionSnapshot::capture(&state, "valid/but-colliding");
        assert_eq!(
            snapshot
                .save_to_dir(temp.path())
                .expect_err("lossy session ID")
                .kind(),
            io::ErrorKind::InvalidInput
        );
    }

    #[test]
    fn stale_cleanup_never_traverses_directory_or_entry_symlinks() {
        use std::os::unix::fs::symlink;

        let temp = tempfile::tempdir().expect("tempdir");
        let outside = temp.path().join("outside");
        fs::create_dir(&outside).expect("outside dir");
        let victim = outside.join("keep.json");
        fs::write(&victim, "do not delete").expect("victim");

        let linked_dir = temp.path().join("linked-sessions");
        symlink(&outside, &linked_dir).expect("directory symlink");
        cleanup_stale_sessions_in(&linked_dir, std::time::Duration::ZERO);
        assert!(victim.exists(), "cleanup traversed the sessions symlink");
        delete_from_dir(&linked_dir, "keep");
        assert!(victim.exists(), "delete traversed the sessions symlink");

        let sessions = temp.path().join("sessions");
        ensure_private_directory(&sessions).expect("sessions dir");
        symlink(&victim, sessions.join("linked.json")).expect("entry symlink");
        cleanup_stale_sessions_in(&sessions, std::time::Duration::ZERO);
        assert!(victim.exists(), "cleanup followed a symlinked JSON entry");
        delete_from_dir(&sessions, "linked");
        assert!(victim.exists(), "delete followed a symlinked JSON entry");
    }

    #[test]
    fn test_detect_environment_plain() {
        // In test context, typically no venv/nix/docker/ssh
        // Just verify it doesn't panic
        let ctx = detect_environment();
        // ctx could be anything depending on test environment
        match ctx {
            EnvironmentContext::Plain
            | EnvironmentContext::PythonVenv { .. }
            | EnvironmentContext::NixShell { .. }
            | EnvironmentContext::Docker { .. }
            | EnvironmentContext::Ssh { .. } => {}
        }
    }
}
