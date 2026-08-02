/// Session persistence: save/restore shell state across terminal restarts.
///
/// When jterm4 spawns jsh with `--session <id>`, jsh restores state from
/// `~/.jsh/sessions/<id>.json`. On exit, jsh saves a snapshot back.
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::fs::{self, File, OpenOptions};
use std::io::{self, Read, Write};
use std::os::unix::fs::{DirBuilderExt, OpenOptionsExt, PermissionsExt};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};

use crate::environment::*;
use crate::parser::ast::CompoundCommand;

/// Snapshot format version. Bump when adding fields (use #[serde(default)] for compat).
const SNAPSHOT_VERSION: u32 = 1;
const MAX_SESSION_ID_BYTES: usize = 128;
const MAX_SESSION_SNAPSHOT_BYTES: usize = 4 * 1024 * 1024;
const MAX_SESSION_JSON_TOKENS: usize = 16 * 1024;
const MAX_SESSION_JSON_DEPTH: usize = 64;
const MAX_SESSION_STRING_BYTES: usize = 256 * 1024;
const MAX_SESSION_TOTAL_TEXT_BYTES: usize = 2 * 1024 * 1024;
const MAX_SESSION_LOGICAL_ITEMS: usize = 8 * 1024;
const MAX_SESSION_MAP_ITEMS: usize = 2 * 1024;
const MAX_SESSION_FUNCTIONS: usize = 256;
const MAX_SESSION_COMPLETION_SPECS: usize = 1024;
const MAX_SESSION_VECTOR_ITEMS: usize = 4 * 1024;
const MAX_SESSION_HOOK_ITEMS: usize = 256;
const MAX_SESSION_DIR_STACK_ITEMS: usize = 256;
const SAFE_FILE_OPEN_FLAGS: i32 =
    nix::libc::O_NOFOLLOW | nix::libc::O_NONBLOCK | nix::libc::O_CLOEXEC;
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
    // Set by jsh-remote.sh for one connection to one machine. Persisting it
    // would carry another host's home directory into a later local session,
    // where `~` would then point somewhere that does not exist here.
    "JSH_REAL_HOME",
    // One-shot Agent child transport. These paths are capabilities for a
    // private snapshot/report directory and must never survive into a command
    // or a persisted terminal session.
    "JSH_AGENT_CHILD_STATE_DIR",
    "JSH_AGENT_CHILD_CWD",
    "JSH_AGENT_CHILD_REPORT",
    "JSH_AGENT_CHILD_COMMAND",
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

fn has_embedded_url_credentials(value: &str) -> bool {
    value.split("://").skip(1).any(|remainder| {
        let authority = remainder.split(['/', '?', '#']).next().unwrap_or_default();
        authority
            .rsplit_once('@')
            .is_some_and(|(userinfo, host)| !userinfo.is_empty() && !host.is_empty())
    })
}

fn has_private_key_material(value: &str) -> bool {
    [
        "-----BEGIN PRIVATE KEY-----",
        "-----BEGIN ENCRYPTED PRIVATE KEY-----",
        "-----BEGIN RSA PRIVATE KEY-----",
        "-----BEGIN EC PRIVATE KEY-----",
        "-----BEGIN OPENSSH PRIVATE KEY-----",
    ]
    .iter()
    .any(|marker| value.contains(marker))
}

fn should_persist_env(name: &str, value: &str) -> bool {
    !SKIP_ENV_VARS.contains(&name)
        && !is_likely_secret_env(name)
        && !has_embedded_url_credentials(value)
        && !has_private_key_material(value)
}

/// Detected environment context for re-activation on restore.
#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub enum EnvironmentContext {
    #[default]
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

/// Serializable snapshot of shell session state.
///
/// `Deserialize` is hand-written (see `SessionSnapshotSeed`) rather than
/// derived: every collection here is charged against a shared budget while it
/// is being built, so a snapshot that claims eight thousand functions is
/// refused at the two hundred and fifty-seventh instead of after all eight
/// thousand exist.
#[derive(Debug, Clone, Serialize)]
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
        .unwrap_or_else(|| {
            // Do not make the first user without a discoverable home own a
            // process-global /tmp/.jsh namespace. A per-UID fallback prevents
            // cross-user denial of service and accidental snapshot sharing.
            // SAFETY: geteuid has no preconditions and only reads process state.
            let uid = unsafe { nix::libc::geteuid() };
            std::env::temp_dir().join(format!("jsh-{uid}"))
        })
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
        .filter(|c| c.is_ascii_alphanumeric() || *c == '-' || *c == '_')
        .collect()
}

fn session_file_in(dir: &Path, session_id: &str) -> io::Result<PathBuf> {
    let safe_id = sanitize_session_id(session_id);
    if safe_id.is_empty() || safe_id != session_id || session_id.len() > MAX_SESSION_ID_BYTES {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "session ID must be at most 128 bytes and contain only ASCII letters, digits, '-' and '_'",
        ));
    }
    Ok(dir.join(format!("{}.json", safe_id)))
}

fn ensure_private_directory(dir: &Path) -> io::Result<()> {
    if !dir.try_exists()? {
        fs::DirBuilder::new()
            .recursive(true)
            .mode(0o700)
            .create(dir)?;
    }
    let directory = OpenOptions::new()
        .read(true)
        .custom_flags(nix::libc::O_DIRECTORY | nix::libc::O_NOFOLLOW | nix::libc::O_CLOEXEC)
        .open(dir)?;
    let metadata = directory.metadata()?;
    if !metadata.is_dir() {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "session path is not a directory",
        ));
    }
    use std::os::unix::fs::MetadataExt;
    // SAFETY: geteuid has no preconditions and only reads process state.
    if metadata.uid() != unsafe { nix::libc::geteuid() } {
        return Err(io::Error::new(
            io::ErrorKind::PermissionDenied,
            "session directory is not owned by the current user",
        ));
    }
    directory.set_permissions(fs::Permissions::from_mode(0o700))
}

fn open_private_file(path: &Path) -> io::Result<File> {
    use std::os::unix::fs::MetadataExt;

    let file = OpenOptions::new()
        .read(true)
        .custom_flags(SAFE_FILE_OPEN_FLAGS)
        .open(path)?;
    let metadata = file.metadata()?;
    if !metadata.is_file() {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "session snapshot must be a regular file",
        ));
    }
    if metadata.nlink() != 1 {
        return Err(io::Error::new(
            io::ErrorKind::PermissionDenied,
            "session snapshot must have exactly one hard link",
        ));
    }
    // SAFETY: geteuid has no preconditions and only reads process state.
    if metadata.uid() != unsafe { nix::libc::geteuid() } {
        return Err(io::Error::new(
            io::ErrorKind::PermissionDenied,
            "session snapshot is not owned by the current user",
        ));
    }
    file.set_permissions(fs::Permissions::from_mode(0o600))?;
    Ok(file)
}

/// Allocation-free structural preflight for untrusted snapshot JSON. The file
/// byte cap alone is insufficient: a few MiB of empty strings/objects can ask
/// serde to allocate hundreds of thousands of collection entries before a
/// post-deserialization validator gets a chance to run.
fn validate_snapshot_json_shape(json: &[u8]) -> io::Result<()> {
    let invalid = |message| io::Error::new(io::ErrorKind::InvalidData, message);
    let mut index = 0usize;
    let mut depth = 0usize;
    let mut tokens = 0usize;
    let mut string_bytes = 0usize;

    while index < json.len() {
        match json[index] {
            b' ' | b'\t' | b'\r' | b'\n' | b',' | b':' => index += 1,
            b'{' | b'[' => {
                depth = depth
                    .checked_add(1)
                    .ok_or_else(|| invalid("session snapshot JSON nesting overflow"))?;
                if depth > MAX_SESSION_JSON_DEPTH {
                    return Err(invalid("session snapshot JSON is nested too deeply"));
                }
                tokens = tokens.saturating_add(1);
                index += 1;
            }
            b'}' | b']' => {
                if depth == 0 {
                    return Err(invalid("session snapshot JSON has unbalanced delimiters"));
                }
                depth -= 1;
                index += 1;
            }
            b'"' => {
                tokens = tokens.saturating_add(1);
                index += 1;
                let start = index;
                let mut escaped = false;
                loop {
                    let Some(&byte) = json.get(index) else {
                        return Err(invalid("session snapshot JSON has an unterminated string"));
                    };
                    if escaped {
                        escaped = false;
                    } else if byte == b'\\' {
                        escaped = true;
                    } else if byte == b'"' {
                        break;
                    }
                    index += 1;
                }
                let len = index.saturating_sub(start);
                if len > MAX_SESSION_STRING_BYTES {
                    return Err(invalid("session snapshot contains an oversized string"));
                }
                string_bytes = string_bytes
                    .checked_add(len)
                    .ok_or_else(|| invalid("session snapshot string budget overflow"))?;
                if string_bytes > MAX_SESSION_TOTAL_TEXT_BYTES {
                    return Err(invalid(
                        "session snapshot exceeds its cumulative text budget",
                    ));
                }
                index += 1;
            }
            _ => {
                // Number, true, false, or null. Full lexical validation remains
                // serde_json's job; this pass only counts allocation-driving
                // values before serde is allowed to allocate them.
                tokens = tokens.saturating_add(1);
                while index < json.len()
                    && !matches!(
                        json[index],
                        b' ' | b'\t' | b'\r' | b'\n' | b',' | b':' | b'{' | b'}' | b'[' | b']'
                    )
                {
                    index += 1;
                }
            }
        }
        if tokens > MAX_SESSION_JSON_TOKENS {
            return Err(invalid("session snapshot contains too many JSON values"));
        }
    }
    if depth != 0 {
        return Err(invalid("session snapshot JSON has unbalanced delimiters"));
    }
    Ok(())
}

struct SnapshotBudget {
    items: usize,
    text_bytes: usize,
}

impl SnapshotBudget {
    fn items(&mut self, count: usize) -> io::Result<()> {
        self.items = self
            .items
            .checked_add(count)
            .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidData, "snapshot item overflow"))?;
        if self.items > MAX_SESSION_LOGICAL_ITEMS {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "session snapshot contains too many collection items",
            ));
        }
        Ok(())
    }

    fn text(&mut self, value: &str) -> io::Result<()> {
        if value.len() > MAX_SESSION_STRING_BYTES {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "session snapshot contains an oversized string",
            ));
        }
        self.text_bytes = self.text_bytes.checked_add(value.len()).ok_or_else(|| {
            io::Error::new(io::ErrorKind::InvalidData, "snapshot text budget overflow")
        })?;
        if self.text_bytes > MAX_SESSION_TOTAL_TEXT_BYTES {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "session snapshot exceeds its cumulative text budget",
            ));
        }
        Ok(())
    }
}

// ---------------------------------------------------------------------------
// Bounded snapshot decoding
// ---------------------------------------------------------------------------
//
// `validate_snapshot_json_shape` already refuses a document that is too deep,
// too long, or has too many tokens, and it does so without allocating. What it
// cannot express is a *per-field* rule: 8 000 functions and 8 000 environment
// variables are the same shape to a structural preflight, and only one of them
// is allowed. Ordinary Serde therefore built the whole structure and met those
// rules afterwards, in `validate_snapshot_logical`.
//
// The seeds below charge as they decode. A map stops at the first entry past
// its limit, a string is measured before it is owned, and the cumulative text
// budget is shared across every field rather than reset per collection — so
// the work done on a snapshot that will be rejected is bounded by the limit it
// broke, not by the size of the file.
//
// `validate_snapshot_logical` stays exactly where it was. It is now a backstop
// rather than the only check, which is what a backstop should be.

type SharedBudget<'a> = &'a std::cell::RefCell<SnapshotBudget>;

/// Charge the shared budget from inside a visitor, where the error type is
/// serde's rather than `io::Error`.
fn charge_text<'a, E: serde::de::Error>(budget: SharedBudget<'a>, value: &str) -> Result<(), E> {
    budget
        .borrow_mut()
        .text(value)
        .map_err(|error| E::custom(error))
}

fn charge_items<'a, E: serde::de::Error>(budget: SharedBudget<'a>, count: usize) -> Result<(), E> {
    budget
        .borrow_mut()
        .items(count)
        .map_err(|error| E::custom(error))
}

/// Reject a collection at the first entry past its limit.
fn check_capacity<E: serde::de::Error>(
    field: &'static str,
    seen: usize,
    limit: usize,
) -> Result<(), E> {
    if seen > limit {
        return Err(E::custom(format_args!(
            "session snapshot field '{field}' exceeds its {limit}-entry restore limit"
        )));
    }
    Ok(())
}

/// A size hint is attacker-controlled, so it is only ever used to preallocate
/// what is already known to be allowed.
fn bounded_capacity(hint: Option<usize>, limit: usize) -> usize {
    hint.unwrap_or(0).min(limit).min(64)
}

/// One string, measured against the shared text budget before it is owned.
struct TextSeed<'a> {
    budget: SharedBudget<'a>,
}

impl<'de, 'a> serde::de::DeserializeSeed<'de> for TextSeed<'a> {
    type Value = String;

    fn deserialize<D: serde::Deserializer<'de>>(self, de: D) -> Result<Self::Value, D::Error> {
        de.deserialize_str(self)
    }
}

impl<'de, 'a> serde::de::Visitor<'de> for TextSeed<'a> {
    type Value = String;

    fn expecting(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "a string within the snapshot text budget")
    }

    fn visit_str<E: serde::de::Error>(self, value: &str) -> Result<Self::Value, E> {
        charge_text(self.budget, value)?;
        Ok(value.to_owned())
    }
}

/// `HashMap<String, String>` with an entry cap; both halves are charged.
struct TextMapSeed<'a> {
    field: &'static str,
    limit: usize,
    budget: SharedBudget<'a>,
}

impl<'de, 'a> serde::de::DeserializeSeed<'de> for TextMapSeed<'a> {
    type Value = HashMap<String, String>;

    fn deserialize<D: serde::Deserializer<'de>>(self, de: D) -> Result<Self::Value, D::Error> {
        de.deserialize_map(self)
    }
}

impl<'de, 'a> serde::de::Visitor<'de> for TextMapSeed<'a> {
    type Value = HashMap<String, String>;

    fn expecting(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "at most {} '{}' entries", self.limit, self.field)
    }

    fn visit_map<A: serde::de::MapAccess<'de>>(self, mut map: A) -> Result<Self::Value, A::Error> {
        let mut out = HashMap::with_capacity(bounded_capacity(map.size_hint(), self.limit));
        while let Some(key) = map.next_key_seed(TextSeed {
            budget: self.budget,
        })? {
            check_capacity::<A::Error>(self.field, out.len() + 1, self.limit)?;
            charge_items::<A::Error>(self.budget, 1)?;
            let value = map.next_value_seed(TextSeed {
                budget: self.budget,
            })?;
            out.insert(key, value);
        }
        Ok(out)
    }
}

/// `Vec<String>` with an item cap.
struct TextVecSeed<'a> {
    field: &'static str,
    limit: usize,
    budget: SharedBudget<'a>,
}

impl<'de, 'a> serde::de::DeserializeSeed<'de> for TextVecSeed<'a> {
    type Value = Vec<String>;

    fn deserialize<D: serde::Deserializer<'de>>(self, de: D) -> Result<Self::Value, D::Error> {
        de.deserialize_seq(self)
    }
}

impl<'de, 'a> serde::de::Visitor<'de> for TextVecSeed<'a> {
    type Value = Vec<String>;

    fn expecting(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "at most {} '{}' items", self.limit, self.field)
    }

    fn visit_seq<A: serde::de::SeqAccess<'de>>(self, mut seq: A) -> Result<Self::Value, A::Error> {
        let mut out = Vec::with_capacity(bounded_capacity(seq.size_hint(), self.limit));
        while let Some(item) = seq.next_element_seed(TextSeed {
            budget: self.budget,
        })? {
            check_capacity::<A::Error>(self.field, out.len() + 1, self.limit)?;
            charge_items::<A::Error>(self.budget, 1)?;
            out.push(item);
        }
        Ok(out)
    }
}

/// `HashMap<String, T>` where `T` is a leaf the structural preflight already
/// bounds. Only the entry count and the key text are charged here; the value
/// is decoded by its own `Deserialize`, which cannot exceed what the preflight
/// admitted for the whole document.
struct CountedMapSeed<'a, T> {
    field: &'static str,
    limit: usize,
    budget: SharedBudget<'a>,
    value: std::marker::PhantomData<T>,
}

impl<'a, T> CountedMapSeed<'a, T> {
    fn new(field: &'static str, limit: usize, budget: SharedBudget<'a>) -> Self {
        Self {
            field,
            limit,
            budget,
            value: std::marker::PhantomData,
        }
    }
}

impl<'de, 'a, T: serde::Deserialize<'de>> serde::de::DeserializeSeed<'de>
    for CountedMapSeed<'a, T>
{
    type Value = HashMap<String, T>;

    fn deserialize<D: serde::Deserializer<'de>>(self, de: D) -> Result<Self::Value, D::Error> {
        de.deserialize_map(self)
    }
}

impl<'de, 'a, T: serde::Deserialize<'de>> serde::de::Visitor<'de> for CountedMapSeed<'a, T> {
    type Value = HashMap<String, T>;

    fn expecting(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "at most {} '{}' entries", self.limit, self.field)
    }

    fn visit_map<A: serde::de::MapAccess<'de>>(self, mut map: A) -> Result<Self::Value, A::Error> {
        let mut out = HashMap::with_capacity(bounded_capacity(map.size_hint(), self.limit));
        while let Some(key) = map.next_key_seed(TextSeed {
            budget: self.budget,
        })? {
            check_capacity::<A::Error>(self.field, out.len() + 1, self.limit)?;
            charge_items::<A::Error>(self.budget, 1)?;
            out.insert(key, map.next_value::<T>()?);
        }
        Ok(out)
    }
}

/// `HashMap<String, Vec<String>>` — shell arrays.
struct ArrayMapSeed<'a> {
    budget: SharedBudget<'a>,
}

impl<'de, 'a> serde::de::DeserializeSeed<'de> for ArrayMapSeed<'a> {
    type Value = HashMap<String, Vec<String>>;

    fn deserialize<D: serde::Deserializer<'de>>(self, de: D) -> Result<Self::Value, D::Error> {
        de.deserialize_map(self)
    }
}

impl<'de, 'a> serde::de::Visitor<'de> for ArrayMapSeed<'a> {
    type Value = HashMap<String, Vec<String>>;

    fn expecting(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "at most {MAX_SESSION_MAP_ITEMS} array entries")
    }

    fn visit_map<A: serde::de::MapAccess<'de>>(self, mut map: A) -> Result<Self::Value, A::Error> {
        let mut out =
            HashMap::with_capacity(bounded_capacity(map.size_hint(), MAX_SESSION_MAP_ITEMS));
        while let Some(key) = map.next_key_seed(TextSeed {
            budget: self.budget,
        })? {
            check_capacity::<A::Error>("arrays", out.len() + 1, MAX_SESSION_MAP_ITEMS)?;
            charge_items::<A::Error>(self.budget, 1)?;
            let values = map.next_value_seed(TextVecSeed {
                field: "arrays",
                limit: MAX_SESSION_VECTOR_ITEMS,
                budget: self.budget,
            })?;
            out.insert(key, values);
        }
        Ok(out)
    }
}

/// `HashMap<String, HashMap<String, String>>` — associative arrays.
struct AssocMapSeed<'a> {
    budget: SharedBudget<'a>,
}

impl<'de, 'a> serde::de::DeserializeSeed<'de> for AssocMapSeed<'a> {
    type Value = HashMap<String, HashMap<String, String>>;

    fn deserialize<D: serde::Deserializer<'de>>(self, de: D) -> Result<Self::Value, D::Error> {
        de.deserialize_map(self)
    }
}

impl<'de, 'a> serde::de::Visitor<'de> for AssocMapSeed<'a> {
    type Value = HashMap<String, HashMap<String, String>>;

    fn expecting(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "at most {MAX_SESSION_MAP_ITEMS} associative arrays")
    }

    fn visit_map<A: serde::de::MapAccess<'de>>(self, mut map: A) -> Result<Self::Value, A::Error> {
        let mut out =
            HashMap::with_capacity(bounded_capacity(map.size_hint(), MAX_SESSION_MAP_ITEMS));
        while let Some(key) = map.next_key_seed(TextSeed {
            budget: self.budget,
        })? {
            check_capacity::<A::Error>("assoc_arrays", out.len() + 1, MAX_SESSION_MAP_ITEMS)?;
            charge_items::<A::Error>(self.budget, 1)?;
            let values = map.next_value_seed(TextMapSeed {
                field: "assoc_arrays",
                limit: MAX_SESSION_MAP_ITEMS,
                budget: self.budget,
            })?;
            out.insert(key, values);
        }
        Ok(out)
    }
}

/// `ShellHooks` — three capped command lists.
struct HooksSeed<'a> {
    budget: SharedBudget<'a>,
}

impl<'de, 'a> serde::de::DeserializeSeed<'de> for HooksSeed<'a> {
    type Value = ShellHooks;

    fn deserialize<D: serde::Deserializer<'de>>(self, de: D) -> Result<Self::Value, D::Error> {
        de.deserialize_map(self)
    }
}

impl<'de, 'a> serde::de::Visitor<'de> for HooksSeed<'a> {
    type Value = ShellHooks;

    fn expecting(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "a hooks object")
    }

    fn visit_map<A: serde::de::MapAccess<'de>>(self, mut map: A) -> Result<Self::Value, A::Error> {
        let mut hooks = ShellHooks::default();
        while let Some(key) = map.next_key::<String>()? {
            let list = |budget, field| TextVecSeed {
                field,
                limit: MAX_SESSION_HOOK_ITEMS,
                budget,
            };
            match key.as_str() {
                "precmd" => hooks.precmd = map.next_value_seed(list(self.budget, "precmd"))?,
                "preexec" => hooks.preexec = map.next_value_seed(list(self.budget, "preexec"))?,
                "chpwd" => hooks.chpwd = map.next_value_seed(list(self.budget, "chpwd"))?,
                // Unknown keys are skipped rather than refused: a snapshot from
                // a build with more hook kinds must still restore the ones this
                // build understands.
                _ => {
                    map.next_value::<serde::de::IgnoredAny>()?;
                }
            }
        }
        Ok(hooks)
    }
}

/// The snapshot itself.
struct SessionSnapshotSeed<'a> {
    budget: SharedBudget<'a>,
}

impl<'de, 'a> serde::de::DeserializeSeed<'de> for SessionSnapshotSeed<'a> {
    type Value = SessionSnapshot;

    fn deserialize<D: serde::Deserializer<'de>>(self, de: D) -> Result<Self::Value, D::Error> {
        de.deserialize_map(self)
    }
}

impl<'de, 'a> serde::de::Visitor<'de> for SessionSnapshotSeed<'a> {
    type Value = SessionSnapshot;

    fn expecting(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "a session snapshot object")
    }

    fn visit_map<A: serde::de::MapAccess<'de>>(self, mut map: A) -> Result<Self::Value, A::Error> {
        use serde::de::Error as _;

        let budget = self.budget;
        let mut version = None;
        let mut session_id = None;
        let mut cwd = None;
        let mut env_vars = None;
        let mut aliases = None;
        let mut functions = None;
        let mut arrays = None;
        let mut assoc_arrays = None;
        let mut shell_opts = None;
        let mut hooks = None;
        let mut traps = None;
        let mut completion_specs = None;
        let mut dir_stack = None;
        let mut editing_mode = None;
        let mut prompt_style = None;
        let mut last_exit_code = None;
        let mut notification_threshold_secs = None;
        let mut environment_context = None;

        let text_map = |field, limit| TextMapSeed {
            field,
            limit,
            budget,
        };

        while let Some(key) = map.next_key::<String>()? {
            // A repeated field is refused rather than resolved to the last one.
            // Two different values for `session_id` in one document is not a
            // snapshot this build wrote, and picking either is a guess.
            macro_rules! once {
                ($slot:ident, $value:expr) => {{
                    if $slot.is_some() {
                        return Err(A::Error::custom(format_args!(
                            "session snapshot repeats field '{}'",
                            key
                        )));
                    }
                    $slot = Some($value);
                }};
            }
            match key.as_str() {
                "version" => once!(version, map.next_value::<u32>()?),
                "session_id" => once!(session_id, map.next_value_seed(TextSeed { budget })?),
                "cwd" => once!(cwd, map.next_value_seed(TextSeed { budget })?),
                "env_vars" => once!(
                    env_vars,
                    map.next_value_seed(text_map("env_vars", MAX_SESSION_MAP_ITEMS))?
                ),
                "aliases" => once!(
                    aliases,
                    map.next_value_seed(text_map("aliases", MAX_SESSION_MAP_ITEMS))?
                ),
                "traps" => once!(
                    traps,
                    map.next_value_seed(text_map("traps", MAX_SESSION_MAP_ITEMS))?
                ),
                "functions" => once!(
                    functions,
                    map.next_value_seed(CountedMapSeed::<CompoundCommand>::new(
                        "functions",
                        MAX_SESSION_FUNCTIONS,
                        budget
                    ))?
                ),
                "completion_specs" => once!(
                    completion_specs,
                    map.next_value_seed(CountedMapSeed::<CompletionSpec>::new(
                        "completion_specs",
                        MAX_SESSION_COMPLETION_SPECS,
                        budget
                    ))?
                ),
                "arrays" => once!(arrays, map.next_value_seed(ArrayMapSeed { budget })?),
                "assoc_arrays" => {
                    once!(assoc_arrays, map.next_value_seed(AssocMapSeed { budget })?)
                }
                "hooks" => once!(hooks, map.next_value_seed(HooksSeed { budget })?),
                "dir_stack" => once!(
                    dir_stack,
                    map.next_value_seed(TextVecSeed {
                        field: "dir_stack",
                        limit: MAX_SESSION_DIR_STACK_ITEMS,
                        budget,
                    })?
                    .into_iter()
                    .map(PathBuf::from)
                    .collect::<Vec<PathBuf>>()
                ),
                "shell_opts" => once!(shell_opts, map.next_value::<ShellOpts>()?),
                "editing_mode" => once!(editing_mode, map.next_value::<EditingMode>()?),
                "prompt_style" => once!(prompt_style, map.next_value::<PromptStyle>()?),
                "last_exit_code" => once!(last_exit_code, map.next_value::<i32>()?),
                "notification_threshold_secs" => {
                    once!(notification_threshold_secs, map.next_value::<u64>()?)
                }
                "environment_context" => {
                    once!(environment_context, map.next_value::<EnvironmentContext>()?)
                }
                // Ignored, not refused: a snapshot written by a newer build must
                // still restore what this one understands. The preflight already
                // bounds whatever is skipped here.
                _ => {
                    map.next_value::<serde::de::IgnoredAny>()?;
                }
            }
        }

        let required = |name: &'static str| A::Error::missing_field(name);
        Ok(SessionSnapshot {
            version: version.ok_or_else(|| required("version"))?,
            session_id: session_id.ok_or_else(|| required("session_id"))?,
            cwd: cwd.ok_or_else(|| required("cwd"))?,
            env_vars: env_vars.ok_or_else(|| required("env_vars"))?,
            aliases: aliases.ok_or_else(|| required("aliases"))?,
            functions: functions.ok_or_else(|| required("functions"))?,
            arrays: arrays.ok_or_else(|| required("arrays"))?,
            assoc_arrays: assoc_arrays.ok_or_else(|| required("assoc_arrays"))?,
            shell_opts: shell_opts.ok_or_else(|| required("shell_opts"))?,
            hooks: hooks.ok_or_else(|| required("hooks"))?,
            traps: traps.ok_or_else(|| required("traps"))?,
            completion_specs: completion_specs.ok_or_else(|| required("completion_specs"))?,
            dir_stack: dir_stack.ok_or_else(|| required("dir_stack"))?,
            editing_mode: editing_mode.ok_or_else(|| required("editing_mode"))?,
            prompt_style: prompt_style.ok_or_else(|| required("prompt_style"))?,
            last_exit_code: last_exit_code.ok_or_else(|| required("last_exit_code"))?,
            notification_threshold_secs: notification_threshold_secs
                .ok_or_else(|| required("notification_threshold_secs"))?,
            // The only field that predates its own introduction.
            environment_context: environment_context.unwrap_or_default(),
        })
    }
}

/// Decode a snapshot with every collection charged as it is built.
///
/// This is the only wire path into a `SessionSnapshot`: the type deliberately
/// does not implement `Deserialize`, so nothing can reach it through ordinary
/// Serde and skip the budget.
pub(crate) fn decode_snapshot(json: &[u8]) -> Result<SessionSnapshot, serde_json::Error> {
    let budget = std::cell::RefCell::new(SnapshotBudget {
        items: 0,
        text_bytes: 0,
    });
    let mut deserializer = serde_json::Deserializer::from_slice(json);
    let snapshot = serde::de::DeserializeSeed::deserialize(
        SessionSnapshotSeed { budget: &budget },
        &mut deserializer,
    )?;
    deserializer.end()?;
    Ok(snapshot)
}

fn validate_snapshot_logical(snapshot: &SessionSnapshot) -> io::Result<()> {
    use std::os::unix::ffi::OsStrExt;

    let limit = |ok: bool, message: &'static str| {
        ok.then_some(())
            .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidData, message))
    };
    limit(
        snapshot.env_vars.len() <= MAX_SESSION_MAP_ITEMS
            && snapshot.aliases.len() <= MAX_SESSION_MAP_ITEMS
            && snapshot.functions.len() <= MAX_SESSION_FUNCTIONS
            && snapshot.arrays.len() <= MAX_SESSION_MAP_ITEMS
            && snapshot.assoc_arrays.len() <= MAX_SESSION_MAP_ITEMS
            && snapshot.traps.len() <= MAX_SESSION_MAP_ITEMS
            && snapshot.shell_opts.tracked_opts.len() <= MAX_SESSION_MAP_ITEMS
            && snapshot.shell_opts.shopt_opts.len() <= MAX_SESSION_MAP_ITEMS
            && snapshot.completion_specs.len() <= MAX_SESSION_COMPLETION_SPECS,
        "session snapshot map exceeds its entry limit",
    )?;
    limit(
        snapshot.hooks.precmd.len() <= MAX_SESSION_HOOK_ITEMS
            && snapshot.hooks.preexec.len() <= MAX_SESSION_HOOK_ITEMS
            && snapshot.hooks.chpwd.len() <= MAX_SESSION_HOOK_ITEMS
            && snapshot.dir_stack.len() <= MAX_SESSION_DIR_STACK_ITEMS,
        "session snapshot list exceeds its entry limit",
    )?;

    let mut budget = SnapshotBudget {
        items: 0,
        text_bytes: 0,
    };
    budget.text(&snapshot.session_id)?;
    budget.text(&snapshot.cwd)?;
    for map in [&snapshot.env_vars, &snapshot.aliases, &snapshot.traps] {
        budget.items(map.len())?;
        for (name, value) in map {
            budget.text(name)?;
            budget.text(value)?;
        }
    }
    budget.items(snapshot.functions.len())?;
    for name in snapshot.functions.keys() {
        budget.text(name)?;
    }
    for (name, values) in &snapshot.arrays {
        limit(
            values.len() <= MAX_SESSION_VECTOR_ITEMS,
            "session array exceeds its item limit",
        )?;
        budget.items(values.len().saturating_add(1))?;
        budget.text(name)?;
        for value in values {
            budget.text(value)?;
        }
    }
    for (name, values) in &snapshot.assoc_arrays {
        limit(
            values.len() <= MAX_SESSION_MAP_ITEMS,
            "session associative array exceeds its item limit",
        )?;
        budget.items(values.len().saturating_add(1))?;
        budget.text(name)?;
        for (key, value) in values {
            budget.text(key)?;
            budget.text(value)?;
        }
    }
    for map in [
        &snapshot.shell_opts.tracked_opts,
        &snapshot.shell_opts.shopt_opts,
    ] {
        budget.items(map.len())?;
        for name in map.keys() {
            budget.text(name)?;
        }
    }
    for hooks in [
        &snapshot.hooks.precmd,
        &snapshot.hooks.preexec,
        &snapshot.hooks.chpwd,
    ] {
        budget.items(hooks.len())?;
        for hook in hooks {
            budget.text(hook)?;
        }
    }
    budget.items(snapshot.completion_specs.len())?;
    for (name, spec) in &snapshot.completion_specs {
        budget.text(name)?;
        budget.text(&spec.command)?;
        if let Some(words) = &spec.word_list {
            limit(
                words.len() <= MAX_SESSION_VECTOR_ITEMS,
                "session completion word list exceeds its item limit",
            )?;
            budget.items(words.len())?;
            for word in words {
                budget.text(word)?;
            }
        }
        for value in [
            spec.function.as_deref(),
            spec.glob_pattern.as_deref(),
            spec.filter_pattern.as_deref(),
            spec.prefix.as_deref(),
            spec.suffix.as_deref(),
        ]
        .into_iter()
        .flatten()
        {
            budget.text(value)?;
        }
    }
    budget.items(snapshot.dir_stack.len())?;
    for path in &snapshot.dir_stack {
        let bytes = path.as_os_str().as_bytes();
        limit(
            bytes.len() <= MAX_SESSION_STRING_BYTES,
            "session directory path exceeds its byte limit",
        )?;
        budget.text_bytes = budget.text_bytes.checked_add(bytes.len()).ok_or_else(|| {
            io::Error::new(io::ErrorKind::InvalidData, "snapshot text budget overflow")
        })?;
    }
    match &snapshot.environment_context {
        EnvironmentContext::Plain => {}
        EnvironmentContext::PythonVenv { virtual_env } => budget.text(virtual_env)?,
        EnvironmentContext::NixShell {
            flake_dir,
            nix_build_top,
        } => {
            if let Some(value) = flake_dir {
                budget.text(value)?;
            }
            if let Some(value) = nix_build_top {
                budget.text(value)?;
            }
        }
        EnvironmentContext::Docker { container_id } => {
            if let Some(value) = container_id {
                budget.text(value)?;
            }
        }
        EnvironmentContext::Ssh { ssh_connection } => budget.text(ssh_connection)?,
    }
    limit(
        budget.text_bytes <= MAX_SESSION_TOTAL_TEXT_BYTES,
        "session snapshot exceeds its cumulative text budget",
    )
}

struct BoundedJsonWriter {
    bytes: Vec<u8>,
}

impl Write for BoundedJsonWriter {
    fn write(&mut self, bytes: &[u8]) -> io::Result<usize> {
        if self.bytes.len().saturating_add(bytes.len()) > MAX_SESSION_SNAPSHOT_BYTES {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "session snapshot exceeds size limit",
            ));
        }
        self.bytes.extend_from_slice(bytes);
        Ok(bytes.len())
    }

    fn flush(&mut self) -> io::Result<()> {
        Ok(())
    }
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
            .filter(|(k, v)| should_persist_env(k, v))
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
            eprintln!("jsh: session restore: failed to cd to {:?}: {e}", self.cwd);
        }

        // Merge env vars: snapshot values override, but keep process-inherited vars for SKIP list
        for (k, v) in &self.env_vars {
            if should_persist_env(k, v) {
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

    pub(crate) fn save_to_dir(&self, dir: &Path) -> io::Result<()> {
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
            .retain(|name, value| should_persist_env(name, value));
        validate_snapshot_logical(&persisted)
            .map_err(|error| io::Error::new(io::ErrorKind::InvalidInput, error))?;
        let mut writer = BoundedJsonWriter { bytes: Vec::new() };
        serde_json::to_writer_pretty(&mut writer, &persisted)
            .map_err(|error| io::Error::new(io::ErrorKind::InvalidInput, error))?;
        validate_snapshot_json_shape(&writer.bytes)
            .map_err(|error| io::Error::new(io::ErrorKind::InvalidInput, error))?;
        let json = writer.bytes;

        let result = (|| {
            let mut file = OpenOptions::new()
                .write(true)
                .create_new(true)
                .custom_flags(nix::libc::O_NOFOLLOW | nix::libc::O_CLOEXEC)
                .mode(0o600)
                .open(&tmp_path)?;
            file.set_permissions(fs::Permissions::from_mode(0o600))?;
            file.write_all(&json)?;
            file.sync_all()?;

            // On Unix, rename replaces an existing regular file atomically.
            // Do not unlink the old snapshot first: doing so creates a window
            // where a crash would leave no recoverable state.
            fs::rename(&tmp_path, &path)?;
            let directory = OpenOptions::new()
                .read(true)
                .custom_flags(nix::libc::O_DIRECTORY | nix::libc::O_NOFOLLOW | nix::libc::O_CLOEXEC)
                .open(dir)?;
            directory.sync_all()?;
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

    pub(crate) fn load_from_dir(
        session_id: &str,
        dir: &Path,
    ) -> Result<Self, Box<dyn std::error::Error>> {
        if dir.exists() {
            ensure_private_directory(dir)?;
        }
        let path = session_file_in(dir, session_id)?;
        let mut file = open_private_file(&path)?;
        if file.metadata()?.len() > MAX_SESSION_SNAPSHOT_BYTES as u64 {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "session snapshot exceeds size limit",
            )
            .into());
        }
        let mut json = Vec::new();
        (&mut file)
            .take((MAX_SESSION_SNAPSHOT_BYTES + 1) as u64)
            .read_to_end(&mut json)?;
        if json.len() > MAX_SESSION_SNAPSHOT_BYTES {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "session snapshot grew beyond size limit while being read",
            )
            .into());
        }
        validate_snapshot_json_shape(&json)?;
        let mut snapshot = decode_snapshot(&json)?;
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
        if snapshot.session_id != session_id {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "session snapshot identity does not match its file name",
            )
            .into());
        }
        validate_snapshot_logical(&snapshot)?;
        // Old version-1 snapshots may predate secret filtering. Never return
        // those stale credentials to the restore path.
        snapshot
            .env_vars
            .retain(|name, value| should_persist_env(name, value));
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
    if open_private_file(&path).is_ok() {
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
                eprintln!("jsh: session restore: venv {virtual_env:?} no longer exists");
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
    use nix::sys::stat::Mode;
    use nix::unistd::mkfifo;
    use std::os::unix::fs::{symlink, PermissionsExt};

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
                !should_persist_env(name, "test-value"),
                "{name} would survive a restore into a different terminal"
            );
        }
        assert!(should_persist_env("EDITOR", "vim"));
    }

    /// A minimal but complete snapshot document, for tests that mutate one
    /// field and expect the decoder to object to that field specifically.
    fn snapshot_json(overrides: &[(&str, String)]) -> String {
        let mut fields: Vec<(String, String)> = vec![
            ("version".into(), SNAPSHOT_VERSION.to_string()),
            ("session_id".into(), "\"probe\"".into()),
            ("cwd".into(), "\"/tmp\"".into()),
            ("env_vars".into(), "{}".into()),
            ("aliases".into(), "{}".into()),
            ("functions".into(), "{}".into()),
            ("arrays".into(), "{}".into()),
            ("assoc_arrays".into(), "{}".into()),
            (
                "shell_opts".into(),
                serde_json::to_string(&ShellOpts::default()).unwrap(),
            ),
            (
                "hooks".into(),
                "{\"precmd\":[],\"preexec\":[],\"chpwd\":[]}".into(),
            ),
            ("traps".into(), "{}".into()),
            ("completion_specs".into(), "{}".into()),
            ("dir_stack".into(), "[]".into()),
            ("editing_mode".into(), "\"Emacs\"".into()),
            ("prompt_style".into(), "\"Auto\"".into()),
            ("last_exit_code".into(), "0".into()),
            ("notification_threshold_secs".into(), "0".into()),
        ];
        for (name, value) in overrides {
            match fields.iter_mut().find(|(field, _)| field == name) {
                Some(slot) => slot.1 = value.clone(),
                None => fields.push(((*name).into(), value.clone())),
            }
        }
        let body: Vec<String> = fields
            .iter()
            .map(|(name, value)| format!("\"{name}\":{value}"))
            .collect();
        format!("{{{}}}", body.join(","))
    }

    fn repeated_map(entries: usize) -> String {
        let body: Vec<String> = (0..entries).map(|i| format!("\"k{i}\":\"v\"")).collect();
        format!("{{{}}}", body.join(","))
    }

    fn spec_json() -> String {
        concat!(
            r#"{"command":"c","word_list":null,"function":null,"directory":false,"#,
            r#""file":false,"glob_pattern":null,"filter_pattern":null,"#,
            r#""prefix":null,"suffix":null}"#
        )
        .to_string()
    }

    #[test]
    fn a_collection_over_its_limit_is_refused_while_decoding() {
        // The point of the seeds. `decode_snapshot` is called directly, without
        // the post-decode audit, so a rejection here proves the limit is
        // enforced as the collection is built rather than after it exists.
        let over = format!(
            "[{}]",
            vec!["\"/x\""; MAX_SESSION_DIR_STACK_ITEMS + 1].join(",")
        );
        let error = decode_snapshot(snapshot_json(&[("dir_stack", over)]).as_bytes())
            .expect_err("a snapshot past the dir_stack limit must not decode");
        let text = error.to_string();
        assert!(text.contains("dir_stack"), "{text}");
        assert!(text.contains("restore limit"), "{text}");

        // The ceilings are per field, not one shared budget: the same number of
        // environment variables is fine, because their limit is far higher.
        let json = snapshot_json(&[("env_vars", repeated_map(MAX_SESSION_DIR_STACK_ITEMS + 1))]);
        assert!(decode_snapshot(json.as_bytes()).is_ok());
    }

    #[test]
    fn every_collection_field_carries_its_own_ceiling() {
        let dir_stack_over = format!(
            "[{}]",
            vec!["\"/x\""; MAX_SESSION_DIR_STACK_ITEMS + 1].join(",")
        );
        let hooks_over = format!(
            "{{\"precmd\":[{}],\"preexec\":[],\"chpwd\":[]}}",
            vec!["\"echo\""; MAX_SESSION_HOOK_ITEMS + 1].join(",")
        );
        let specs_over = {
            let body: Vec<String> = (0..=MAX_SESSION_COMPLETION_SPECS)
                .map(|i| format!("\"k{i}\":{}", spec_json()))
                .collect();
            format!("{{{}}}", body.join(","))
        };
        let arrays_over = {
            let body: Vec<String> = (0..=MAX_SESSION_MAP_ITEMS)
                .map(|i| format!("\"k{i}\":[]"))
                .collect();
            format!("{{{}}}", body.join(","))
        };
        let cases: Vec<(&str, String)> = vec![
            ("env_vars", repeated_map(MAX_SESSION_MAP_ITEMS + 1)),
            ("aliases", repeated_map(MAX_SESSION_MAP_ITEMS + 1)),
            ("traps", repeated_map(MAX_SESSION_MAP_ITEMS + 1)),
            ("completion_specs", specs_over),
            ("arrays", arrays_over),
            ("dir_stack", dir_stack_over),
            ("hooks", hooks_over),
        ];
        for (field, value) in cases {
            let json = snapshot_json(&[(field, value)]);
            let error = decode_snapshot(json.as_bytes())
                .err()
                .unwrap_or_else(|| panic!("{field} decoded despite exceeding its limit"));
            assert!(
                error.to_string().contains("restore limit"),
                "{field}: {error}"
            );
        }
    }

    #[test]
    fn a_collection_at_its_limit_still_decodes() {
        // The boundary matters as much as the rejection: an off-by-one here
        // would refuse snapshots this build itself wrote.
        let at_limit = format!(
            "[{}]",
            vec!["\"/x\""; MAX_SESSION_DIR_STACK_ITEMS].join(",")
        );
        let snapshot = decode_snapshot(snapshot_json(&[("dir_stack", at_limit)]).as_bytes())
            .expect("a collection exactly at its limit must decode");
        assert_eq!(snapshot.dir_stack.len(), MAX_SESSION_DIR_STACK_ITEMS);
    }

    #[test]
    fn a_repeated_field_is_refused_rather_than_resolved() {
        // Two values for one field is not a document this build wrote, and
        // taking either one is a guess about which the writer meant.
        let json =
            snapshot_json(&[]).replace("\"cwd\":\"/tmp\"", "\"cwd\":\"/tmp\",\"cwd\":\"/etc\"");
        let error = decode_snapshot(json.as_bytes()).expect_err("a repeated field must not decode");
        assert!(error.to_string().contains("repeats field"), "{error}");
    }

    #[test]
    fn an_unknown_field_is_ignored_so_newer_snapshots_still_restore() {
        let json = snapshot_json(&[("a_field_from_the_future", "{\"nested\":[1,2,3]}".into())]);
        let snapshot = decode_snapshot(json.as_bytes()).expect("unknown fields are skipped");
        assert_eq!(snapshot.session_id, "probe");
    }

    #[test]
    fn environment_context_defaults_but_the_rest_is_required() {
        // The one field that postdates the schema it lives in.
        let snapshot = decode_snapshot(snapshot_json(&[]).as_bytes()).expect("decode");
        assert!(matches!(
            snapshot.environment_context,
            EnvironmentContext::Plain
        ));

        let without_cwd = snapshot_json(&[]).replace("\"cwd\":\"/tmp\",", "");
        let error = decode_snapshot(without_cwd.as_bytes())
            .expect_err("a missing required field must not decode");
        assert!(error.to_string().contains("cwd"), "{error}");
    }

    #[test]
    fn the_text_budget_is_shared_across_fields_not_reset_per_collection() {
        // Two collections that are each individually fine, and together are not.
        let half = "x".repeat(MAX_SESSION_STRING_BYTES);
        let entries = |count: usize| -> String {
            let body: Vec<String> = (0..count).map(|i| format!("\"k{i}\":\"{half}\"")).collect();
            format!("{{{}}}", body.join(","))
        };
        let per_field = MAX_SESSION_TOTAL_TEXT_BYTES / MAX_SESSION_STRING_BYTES;
        let json = snapshot_json(&[
            ("env_vars", entries(per_field)),
            ("aliases", entries(per_field)),
        ]);
        let error = decode_snapshot(json.as_bytes())
            .expect_err("the cumulative text budget must span fields");
        assert!(error.to_string().contains("text budget"), "{error}");
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
        let restored = decode_snapshot(json.as_bytes()).expect("deserialize");

        assert_eq!(restored.session_id, "test-roundtrip");
        assert_eq!(restored.aliases.get("ll"), Some(&"ls -la".to_string()));
        assert_eq!(restored.env_vars.get("MY_VAR"), Some(&"hello".to_string()));
        assert!(restored.shell_opts.extglob);
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
            "REDIS_URL".to_string(),
            "redis://cache-user:cache-password@example.invalid/0".to_string(),
        );
        state.env_vars.insert(
            "PUBLIC_URL".to_string(),
            "https://example.invalid/public".to_string(),
        );
        state.env_vars.insert(
            "CERT_BLOB".to_string(),
            "-----BEGIN OPENSSH PRIVATE KEY-----\nsecret".to_string(),
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
        assert!(!snapshot.env_vars.contains_key("REDIS_URL"));
        assert!(!snapshot.env_vars.contains_key("CERT_BLOB"));
        assert_eq!(
            snapshot.env_vars.get("PUBLIC_URL"),
            Some(&"https://example.invalid/public".to_string())
        );
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

        let snapshot = SessionSnapshot::capture(&state, "终端");
        assert_eq!(
            snapshot
                .save_to_dir(temp.path())
                .expect_err("non-ASCII session ID")
                .kind(),
            io::ErrorKind::InvalidInput
        );
    }

    #[test]
    fn stale_cleanup_never_traverses_directory_or_entry_symlinks() {
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
    fn snapshot_links_and_fifos_are_rejected_without_blocking_or_chmodding_targets() {
        let temp = tempfile::tempdir().expect("tempdir");
        let dir = temp.path().join("sessions");
        ensure_private_directory(&dir).expect("session dir");
        let victim = temp.path().join("victim");
        fs::write(&victim, "keep me\n").expect("victim");
        fs::set_permissions(&victim, fs::Permissions::from_mode(0o640)).expect("victim mode");

        symlink(&victim, dir.join("linked.json")).expect("snapshot symlink");
        assert!(SessionSnapshot::load_from_dir("linked", &dir).is_err());

        fs::hard_link(&victim, dir.join("hard.json")).expect("snapshot hard link");
        assert!(SessionSnapshot::load_from_dir("hard", &dir).is_err());
        delete_from_dir(&dir, "hard");
        assert!(dir.join("hard.json").exists());

        mkfifo(&dir.join("fifo.json"), Mode::S_IRUSR | Mode::S_IWUSR).expect("snapshot fifo");
        assert!(SessionSnapshot::load_from_dir("fifo", &dir).is_err());

        assert_eq!(fs::read_to_string(&victim).expect("victim"), "keep me\n");
        assert_eq!(
            fs::metadata(&victim)
                .expect("victim metadata")
                .permissions()
                .mode()
                & 0o777,
            0o640
        );
    }

    #[test]
    fn atomic_snapshot_save_replaces_a_symlink_without_touching_its_target() {
        let temp = tempfile::tempdir().expect("tempdir");
        let dir = temp.path().join("sessions");
        ensure_private_directory(&dir).expect("session dir");
        let victim = temp.path().join("victim");
        fs::write(&victim, "keep me\n").expect("victim");
        fs::set_permissions(&victim, fs::Permissions::from_mode(0o640)).expect("victim mode");
        symlink(&victim, dir.join("replace-link.json")).expect("snapshot symlink");
        let snapshot = SessionSnapshot::capture(&ShellState::new(false), "replace-link");

        snapshot.save_to_dir(&dir).expect("safe atomic replacement");

        assert!(!fs::symlink_metadata(dir.join("replace-link.json"))
            .expect("snapshot metadata")
            .file_type()
            .is_symlink());
        assert_eq!(fs::read_to_string(&victim).expect("victim"), "keep me\n");
        assert_eq!(
            fs::metadata(&victim)
                .expect("victim metadata")
                .permissions()
                .mode()
                & 0o777,
            0o640
        );
    }

    #[test]
    fn snapshot_size_limits_preserve_the_last_good_file() {
        let temp = tempfile::tempdir().expect("tempdir");
        let dir = temp.path().join("sessions");
        let state = ShellState::new(false);
        let mut snapshot = SessionSnapshot::capture(&state, "bounded");
        snapshot.aliases.insert("keep".into(), "true".into());
        snapshot.save_to_dir(&dir).expect("initial snapshot");

        snapshot
            .aliases
            .insert("oversized".into(), "x".repeat(MAX_SESSION_SNAPSHOT_BYTES));
        assert!(snapshot.save_to_dir(&dir).is_err());

        let restored = SessionSnapshot::load_from_dir("bounded", &dir).expect("last good");
        assert_eq!(
            restored.aliases.get("keep").map(String::as_str),
            Some("true")
        );
        assert!(!restored.aliases.contains_key("oversized"));

        let path = session_file_in(&dir, "bounded").expect("snapshot path");
        let file = OpenOptions::new()
            .write(true)
            .truncate(true)
            .open(&path)
            .expect("oversized fixture");
        file.set_len((MAX_SESSION_SNAPSHOT_BYTES + 1) as u64)
            .expect("sparse snapshot");
        drop(file);
        assert!(SessionSnapshot::load_from_dir("bounded", &dir).is_err());
    }

    #[test]
    fn snapshot_identity_must_match_the_requested_session() {
        let temp = tempfile::tempdir().expect("tempdir");
        let dir = temp.path().join("sessions");
        ensure_private_directory(&dir).expect("session dir");
        let snapshot = SessionSnapshot::capture(&ShellState::new(false), "another-session");
        let path = session_file_in(&dir, "requested-session").expect("snapshot path");
        fs::write(&path, serde_json::to_vec(&snapshot).expect("serialize")).expect("fixture");
        fs::set_permissions(&path, fs::Permissions::from_mode(0o600)).expect("fixture mode");

        let error = SessionSnapshot::load_from_dir("requested-session", &dir)
            .expect_err("identity mismatch");
        assert!(error.to_string().contains("identity does not match"));
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
