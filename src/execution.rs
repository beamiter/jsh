//! Structured command execution journal shared with terminal emulators.
//!
//! The journal is deliberately separate from command history. It is an
//! append-only JSONL event stream so jsh and a terminal can safely contribute
//! metadata without redirecting a child's stdout/stderr away from its PTY.

use nix::fcntl::{Flock, FlockArg};
use serde::{Deserialize, Serialize};
use std::borrow::Cow;
use std::collections::{BTreeMap, HashMap};
use std::fs::{self, File, OpenOptions};
use std::io::{self, BufRead, BufReader, Read, Seek, SeekFrom, Write};
use std::os::unix::ffi::OsStrExt;
use std::os::unix::fs::{DirBuilderExt, OpenOptionsExt, PermissionsExt};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

pub const EXECUTION_JOURNAL_VERSION: u32 = 1;
pub const MAX_COMMAND_BYTES: usize = 64 * 1024;
pub const MAX_CWD_BYTES: usize = 4 * 1024;
pub const MAX_OUTPUT_BYTES: usize = 256 * 1024;
pub const MAX_EVENT_LINE_BYTES: usize = 1024 * 1024;
pub const MAX_JOURNAL_FILE_BYTES: u64 = 32 * 1024 * 1024;
/// Maximum physical JSONL records inspected during one journal read.
///
/// This exceeds the number of shortest recognized v1 events that fit in the
/// byte window, while bounding CPU spent rejecting malformed short lines.
pub const MAX_JOURNAL_EVENT_LINES: usize = 512 * 1024;
pub const COMPACTED_JOURNAL_TARGET_BYTES: usize = 24 * 1024 * 1024;
pub const MAX_RETAINED_EXECUTIONS: usize = 2_000;
pub(crate) const MAX_JOURNAL_PATH_BYTES: usize = 16 * 1024;
pub(crate) const JOURNAL_LOCK_FILE_NAME: &str = "executions.lock";
/// Maximum bytes in the execution identifier shared with terminal OSC 133
/// metadata and terminal-produced output events.
pub const MAX_EXECUTION_ID_BYTES: usize = 192;
/// Maximum bytes in a persistent terminal-session identifier.
pub const MAX_SESSION_ID_BYTES: usize = 128;
const MAX_JOURNAL_READ_BYTES: u64 = MAX_JOURNAL_FILE_BYTES + MAX_EVENT_LINE_BYTES as u64 + 1;
const JOURNAL_LOCK_TIMEOUT: Duration = Duration::from_secs(2);
const JOURNAL_LOCK_RETRY: Duration = Duration::from_millis(10);
const SAFE_FILE_OPEN_FLAGS: i32 =
    nix::libc::O_NOFOLLOW | nix::libc::O_NONBLOCK | nix::libc::O_CLOEXEC;

static TEMP_FILE_COUNTER: AtomicU64 = AtomicU64::new(0);

#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
pub struct ExecutionOutput {
    pub text: String,
    pub truncated: bool,
    pub total_bytes: u64,
    pub captured_at_ms: u64,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
pub struct ExecutionRecord {
    pub id: String,
    pub session_id: Option<String>,
    pub seq: u64,
    pub command: String,
    pub command_truncated: bool,
    pub cwd: String,
    pub started_at_ms: u64,
    pub exit_code: Option<i32>,
    pub duration_ms: Option<u64>,
    pub cwd_after: Option<String>,
    pub ended_at_ms: Option<u64>,
    pub output: Option<ExecutionOutput>,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(tag = "event")]
enum ExecutionEvent {
    #[serde(rename = "start")]
    Start(StartEvent),
    #[serde(rename = "finish")]
    Finish {
        #[serde(alias = "rsh_execution_version")]
        jsh_execution_version: u32,
        id: String,
        exit_code: i32,
        duration_ms: u64,
        cwd_after: String,
        ended_at_ms: u64,
    },
    #[serde(rename = "output")]
    Output {
        #[serde(alias = "rsh_execution_version")]
        jsh_execution_version: u32,
        id: String,
        text: String,
        truncated: bool,
        total_bytes: u64,
        captured_at_ms: u64,
    },
    /// Durable ambiguity marker written only by compaction. Legacy v1 readers
    /// reject this additive event and therefore leave the slot unknown too.
    #[serde(rename = "conflict")]
    Conflict(ConflictEvent),
}

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
struct StartEvent {
    #[serde(alias = "rsh_execution_version")]
    jsh_execution_version: u32,
    id: String,
    session_id: Option<String>,
    seq: u64,
    command: String,
    #[serde(default)]
    command_truncated: bool,
    cwd: String,
    started_at_ms: u64,
}

#[derive(Debug, Deserialize)]
struct EventIdentity<'a> {
    #[serde(borrow)]
    event: Cow<'a, str>,
    #[serde(alias = "rsh_execution_version")]
    jsh_execution_version: u32,
    #[serde(borrow)]
    id: Cow<'a, str>,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
struct ConflictEvent {
    #[serde(alias = "rsh_execution_version")]
    jsh_execution_version: u32,
    id: String,
    slot: ConflictSlot,
}

#[derive(Clone, Copy, Debug, Deserialize, Serialize)]
#[serde(rename_all = "snake_case")]
enum ConflictSlot {
    Finish,
    Output,
}

impl ExecutionEvent {
    fn version(&self) -> u32 {
        match self {
            Self::Start(event) => event.jsh_execution_version,
            Self::Finish {
                jsh_execution_version,
                ..
            }
            | Self::Output {
                jsh_execution_version,
                ..
            } => *jsh_execution_version,
            Self::Conflict(event) => event.jsh_execution_version,
        }
    }
}

/// Extract the identity barrier without decoding any other Start metadata.
/// This keeps malformed replacement fields from leaving stale state active.
fn recognized_v1_start_id(line: &[u8]) -> Option<Cow<'_, str>> {
    let identity = serde_json::from_slice::<EventIdentity<'_>>(line).ok()?;
    (identity.event == "start"
        && identity.jsh_execution_version == EXECUTION_JOURNAL_VERSION
        && is_valid_execution_id(&identity.id))
    .then_some(identity.id)
}

#[derive(Debug)]
struct FoldedExecution {
    record: ExecutionRecord,
    finish_conflicted: bool,
    output_conflicted: bool,
}

impl FoldedExecution {
    fn new(record: ExecutionRecord) -> Self {
        Self {
            record,
            finish_conflicted: false,
            output_conflicted: false,
        }
    }

    fn apply_finish(
        &mut self,
        exit_code: i32,
        duration_ms: u64,
        cwd_after: String,
        ended_at_ms: u64,
    ) {
        if self.finish_conflicted {
            return;
        }
        if self.record.exit_code.is_none() {
            self.record.exit_code = Some(exit_code);
            self.record.duration_ms = Some(duration_ms);
            self.record.cwd_after = Some(cwd_after);
            self.record.ended_at_ms = Some(ended_at_ms);
        } else if self.record.exit_code != Some(exit_code)
            || self.record.duration_ms != Some(duration_ms)
            || self.record.cwd_after.as_deref() != Some(cwd_after.as_str())
            || self.record.ended_at_ms != Some(ended_at_ms)
        {
            self.poison_finish();
        }
    }

    fn apply_output(&mut self, output: ExecutionOutput) {
        if self.output_conflicted {
            return;
        }
        match self.record.output.as_ref() {
            None => self.record.output = Some(output),
            Some(existing) if existing == &output => {}
            Some(_) => self.poison_output(),
        }
    }

    fn poison_finish(&mut self) {
        self.record.exit_code = None;
        self.record.duration_ms = None;
        self.record.cwd_after = None;
        self.record.ended_at_ms = None;
        self.finish_conflicted = true;
    }

    fn poison_output(&mut self) {
        self.record.output = None;
        self.output_conflicted = true;
    }
}

#[derive(Clone, Debug)]
pub struct ExecutionJournal {
    path: PathBuf,
    lock_path: PathBuf,
    harden_existing_parent: bool,
}

impl ExecutionJournal {
    /// Return the configured journal. `JSH_EXECUTION_JOURNAL` is only an
    /// enable/disable switch; a custom location uses
    /// `JSH_EXECUTION_JOURNAL_PATH`.
    pub fn configured() -> Option<Self> {
        // The journal moved from ~/.local/state/rsh to ~/.local/state/jsh with
        // the 0.2.0 rename, so `exec last-failed` and the terminal integrations
        // would otherwise start from an empty stream.
        crate::config::migrate_legacy_rsh_data();

        if std::env::var("JSH_EXECUTION_JOURNAL")
            .ok()
            .as_deref()
            .is_some_and(env_value_is_false)
        {
            return None;
        }
        let override_path = std::env::var_os("JSH_EXECUTION_JOURNAL_PATH");
        let harden_existing_parent =
            journal_parent_hardening_for_override(override_path.as_deref());
        let path = select_journal_path(override_path)?;
        Some(Self::with_path_policy(path, harden_existing_parent))
    }

    pub fn with_path(path: PathBuf) -> Self {
        Self::with_path_policy(path, false)
    }

    fn with_path_policy(path: PathBuf, harden_existing_parent: bool) -> Self {
        let lock_path = path
            .parent()
            .unwrap_or_else(|| Path::new("."))
            .join(JOURNAL_LOCK_FILE_NAME);
        Self {
            path,
            lock_path,
            harden_existing_parent,
        }
    }

    pub fn path(&self) -> &Path {
        &self.path
    }

    #[allow(clippy::too_many_arguments)]
    pub fn record_start(
        &self,
        id: &str,
        session_id: Option<&str>,
        seq: u64,
        command: &str,
        cwd: &str,
        started_at_ms: u64,
    ) -> io::Result<()> {
        let (command, command_truncated) = bounded_text(command, MAX_COMMAND_BYTES);
        validate_command_text(&command, MAX_COMMAND_BYTES)?;
        self.append_event(ExecutionEvent::Start(StartEvent {
            jsh_execution_version: EXECUTION_JOURNAL_VERSION,
            id: validate_execution_id(id)?.to_string(),
            session_id: match session_id {
                Some(id) => Some(validate_session_id(id)?.to_string()),
                None => None,
            },
            seq,
            command,
            command_truncated,
            cwd: validate_cwd(cwd)?.to_string(),
            started_at_ms,
        }))
    }

    pub fn record_finish(
        &self,
        id: &str,
        exit_code: i32,
        duration_ms: u64,
        cwd_after: &str,
        ended_at_ms: u64,
    ) -> io::Result<()> {
        self.append_event(ExecutionEvent::Finish {
            jsh_execution_version: EXECUTION_JOURNAL_VERSION,
            id: validate_execution_id(id)?.to_string(),
            exit_code,
            duration_ms,
            cwd_after: validate_cwd(cwd_after)?.to_string(),
            ended_at_ms,
        })
    }

    /// Append terminal-rendered output. This is used by terminal integrations;
    /// jsh itself must not pipe child output because doing so breaks TTY
    /// detection, job control, and full-screen applications.
    pub fn record_output(
        &self,
        id: &str,
        text: &str,
        truncated: bool,
        total_bytes: u64,
        captured_at_ms: u64,
    ) -> io::Result<()> {
        let observed_bytes = u64::try_from(text.len()).unwrap_or(u64::MAX);
        let total_bytes = total_bytes.max(observed_bytes);
        let (mut text, limited) = bounded_text(text, MAX_OUTPUT_BYTES);
        let (truncated, total_bytes) =
            normalize_output_metadata(text.len(), truncated || limited, total_bytes);
        let mut event = ExecutionEvent::Output {
            jsh_execution_version: EXECUTION_JOURNAL_VERSION,
            id: validate_execution_id(id)?.to_string(),
            text: text.clone(),
            truncated,
            total_bytes,
            captured_at_ms,
        };
        // JSON escaping can expand control-heavy text beyond the line limit.
        // Shrink once more rather than writing an unreadable oversized event.
        if serde_json::to_vec(&event).map_err(io::Error::other)?.len() > MAX_EVENT_LINE_BYTES {
            (text, _) = bounded_text(&text, MAX_OUTPUT_BYTES / 2);
            event = ExecutionEvent::Output {
                jsh_execution_version: EXECUTION_JOURNAL_VERSION,
                id: id.to_string(),
                text,
                truncated: true,
                total_bytes,
                captured_at_ms,
            };
        }
        self.append_event(event)
    }

    /// Fold the append-only event stream into one record per execution.
    /// Malformed, oversized, unknown-version, and orphan events are ignored.
    pub fn records(&self) -> io::Result<Vec<ExecutionRecord>> {
        self.ensure_valid_path()?;
        if matches!(
            fs::symlink_metadata(&self.path),
            Err(error) if error.kind() == io::ErrorKind::NotFound
        ) {
            return Ok(Vec::new());
        }
        let _lock = self.lock(FlockArg::LockShared)?;
        match read_records(&self.path) {
            Ok(records) => Ok(records.into_iter().map(|folded| folded.record).collect()),
            Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(Vec::new()),
            Err(error) => Err(error),
        }
    }

    /// Return records in chronological order, optionally scoped to one
    /// terminal session. A zero limit returns no records.
    pub fn list(&self, session_id: Option<&str>, limit: usize) -> io::Result<Vec<ExecutionRecord>> {
        let mut records = self.records()?;
        if let Some(session_id) = session_id {
            records.retain(|record| record.session_id.as_deref() == Some(session_id));
        }
        let keep_from = records.len().saturating_sub(limit);
        Ok(records.split_off(keep_from))
    }

    pub fn show(&self, id: &str) -> io::Result<Option<ExecutionRecord>> {
        self.get(id)
    }

    pub fn get(&self, id: &str) -> io::Result<Option<ExecutionRecord>> {
        Ok(self.records()?.into_iter().find(|record| record.id == id))
    }

    pub fn last_failed(&self) -> io::Result<Option<ExecutionRecord>> {
        Ok(self
            .records()?
            .into_iter()
            .rev()
            .find(|record| record.exit_code.is_some_and(|code| code != 0)))
    }

    fn append_event(&self, event: ExecutionEvent) -> io::Result<()> {
        let mut encoded = serde_json::to_vec(&event).map_err(io::Error::other)?;
        if encoded.len() > MAX_EVENT_LINE_BYTES {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "execution journal event exceeds size limit",
            ));
        }
        encoded.push(b'\n');

        let _lock = self.lock(FlockArg::LockExclusive)?;
        let mut file = open_regular_file(&self.path, true, true, true)?;
        set_private_open_file_permissions(&file)?;
        if file.metadata()?.len() > MAX_JOURNAL_FILE_BYTES {
            drop(file);
            compact_unlocked(&self.path)?;
            file = open_regular_file(&self.path, true, true, true)?;
            set_private_open_file_permissions(&file)?;
        }
        // A power loss can leave the prior write without its JSONL newline.
        // Separate that partial record before appending so one torn tail does
        // not consume the first valid event after recovery.
        if file.metadata()?.len() != 0 {
            file.seek(SeekFrom::End(-1))?;
            let mut last = [0_u8; 1];
            file.read_exact(&mut last)?;
            if last[0] != b'\n' {
                file.write_all(b"\n")?;
            }
        }
        file.write_all(&encoded)?;
        if file.metadata()?.len() > MAX_JOURNAL_FILE_BYTES {
            drop(file);
            compact_unlocked(&self.path)?;
        }
        Ok(())
    }

    fn lock(&self, arg: FlockArg) -> io::Result<JournalLock> {
        self.ensure_valid_path()?;
        let directory = ensure_journal_parent(&self.path, self.harden_existing_parent)?;
        // Updated peers lock the directory before opening the sidecar, so one
        // cannot rename it and acquire a different lock inode mid-operation.
        let directory = flock_with_timeout(directory, arg, JOURNAL_LOCK_TIMEOUT)?;
        let file = OpenOptions::new()
            .read(true)
            .write(true)
            .create(true)
            .truncate(false)
            .custom_flags(SAFE_FILE_OPEN_FLAGS)
            .mode(0o600)
            .open(&self.lock_path)?;
        ensure_regular_file(&file, &self.lock_path)?;
        set_private_open_file_permissions(&file)?;
        let file = flock_with_timeout(file, arg, JOURNAL_LOCK_TIMEOUT)?;
        Ok(JournalLock {
            _directory: directory,
            _file: file,
        })
    }

    fn ensure_valid_path(&self) -> io::Result<()> {
        if is_valid_journal_path(&self.path) {
            Ok(())
        } else {
            Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "execution journal path is invalid or collides with its lock sidecar",
            ))
        }
    }
}

struct JournalLock {
    _directory: Flock<File>,
    _file: Flock<File>,
}

fn flock_with_timeout(mut file: File, arg: FlockArg, timeout: Duration) -> io::Result<Flock<File>> {
    let nonblocking = match arg {
        FlockArg::LockShared | FlockArg::LockSharedNonblock => FlockArg::LockSharedNonblock,
        FlockArg::LockExclusive | FlockArg::LockExclusiveNonblock => {
            FlockArg::LockExclusiveNonblock
        }
        FlockArg::Unlock => FlockArg::Unlock,
        _ => {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "unsupported execution journal lock mode",
            ));
        }
    };
    let started = Instant::now();
    loop {
        match Flock::lock(file, nonblocking) {
            Ok(lock) => return Ok(lock),
            Err((returned, errno)) => {
                file = returned;
                if errno != nix::errno::Errno::EAGAIN {
                    return Err(io::Error::from_raw_os_error(errno as i32));
                }
                if started.elapsed() >= timeout {
                    return Err(io::Error::new(
                        io::ErrorKind::WouldBlock,
                        "timed out waiting for execution journal lock",
                    ));
                }
                let remaining = timeout
                    .checked_sub(started.elapsed())
                    .unwrap_or(Duration::ZERO);
                std::thread::sleep(JOURNAL_LOCK_RETRY.min(remaining));
            }
        }
    }
}

fn ensure_journal_parent(path: &Path, harden_existing: bool) -> io::Result<File> {
    let parent = path
        .parent()
        .filter(|parent| !parent.as_os_str().is_empty())
        .unwrap_or_else(|| Path::new("."));
    let existed = match fs::symlink_metadata(parent) {
        Ok(_) => true,
        Err(error) if error.kind() == io::ErrorKind::NotFound => false,
        Err(error) => return Err(error),
    };
    if !existed {
        fs::DirBuilder::new()
            .recursive(true)
            .mode(0o700)
            .create(parent)?;
    }
    let directory = OpenOptions::new()
        .read(true)
        .custom_flags(nix::libc::O_DIRECTORY | nix::libc::O_NOFOLLOW | nix::libc::O_CLOEXEC)
        .open(parent)?;
    ensure_owned_directory(&directory, parent)?;
    if !existed || harden_existing {
        directory.set_permissions(fs::Permissions::from_mode(0o700))?;
    }
    ensure_unshared_directory(&directory, parent)?;
    Ok(directory)
}

fn open_regular_file(path: &Path, read: bool, append: bool, create: bool) -> io::Result<File> {
    let file = OpenOptions::new()
        .read(read)
        .append(append)
        .create(create)
        .truncate(false)
        .custom_flags(SAFE_FILE_OPEN_FLAGS)
        .mode(0o600)
        .open(path)?;
    ensure_regular_file(&file, path)?;
    Ok(file)
}

fn ensure_regular_file(file: &File, path: &Path) -> io::Result<()> {
    use std::os::unix::fs::MetadataExt;

    let metadata = file.metadata()?;
    if !metadata.is_file() {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            format!("{path:?} is not a regular file"),
        ));
    }
    if metadata.nlink() != 1 {
        return Err(io::Error::new(
            io::ErrorKind::PermissionDenied,
            format!("{path:?} must have exactly one hard link"),
        ));
    }
    // SAFETY: geteuid has no preconditions and only reads process state.
    if metadata.uid() != unsafe { nix::libc::geteuid() } {
        return Err(io::Error::new(
            io::ErrorKind::PermissionDenied,
            format!("{path:?} is not owned by the current user"),
        ));
    }
    // Do not "repair" a file another account can already have open for
    // writing: chmod cannot revoke that existing descriptor. Owner-only files
    // with extra read bits are tightened to 0600 by the caller after this
    // integrity gate.
    if metadata.mode() & 0o022 != 0 {
        return Err(io::Error::new(
            io::ErrorKind::PermissionDenied,
            format!("{path:?} is writable by another user or group"),
        ));
    }
    Ok(())
}

fn ensure_owned_directory(file: &File, path: &Path) -> io::Result<()> {
    use std::os::unix::fs::MetadataExt;

    let metadata = file.metadata()?;
    if !metadata.is_dir() {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            format!("{path:?} is not a directory"),
        ));
    }
    // SAFETY: geteuid has no preconditions and only reads process state.
    if metadata.uid() != unsafe { nix::libc::geteuid() } {
        return Err(io::Error::new(
            io::ErrorKind::PermissionDenied,
            format!("{path:?} is not owned by the current user"),
        ));
    }
    Ok(())
}

/// Shared runtime/doctor rule for a journal namespace that no peer can replace.
pub(crate) fn journal_parent_mode_is_safe(mode: u32) -> bool {
    mode & 0o022 == 0
}

fn ensure_unshared_directory(file: &File, path: &Path) -> io::Result<()> {
    use std::os::unix::fs::MetadataExt;

    let mode = file.metadata()?.mode();
    if !journal_parent_mode_is_safe(mode) {
        return Err(io::Error::new(
            io::ErrorKind::PermissionDenied,
            format!("{path:?} is writable by another user or group"),
        ));
    }
    Ok(())
}

fn set_private_open_file_permissions(file: &File) -> io::Result<()> {
    file.set_permissions(fs::Permissions::from_mode(0o600))
}

pub fn default_journal_path() -> Option<PathBuf> {
    let state_dir = dirs::state_dir()
        .or_else(|| dirs::home_dir().map(|home| home.join(".local").join("state")));
    state_dir.map(|state_dir| state_dir.join("jsh").join("executions.jsonl"))
}

fn journal_parent_hardening_for_override(override_path: Option<&std::ffi::OsStr>) -> bool {
    override_path.is_none() || override_path.is_some_and(std::ffi::OsStr::is_empty)
}

fn select_journal_path(override_path: Option<std::ffi::OsString>) -> Option<PathBuf> {
    let path = match override_path {
        Some(path) if path.is_empty() => default_journal_path()?,
        Some(path) => {
            let path = PathBuf::from(path);
            if !path.is_absolute() {
                return None;
            }
            path
        }
        None => default_journal_path()?,
    };
    is_valid_journal_path(&path).then_some(path)
}

/// Whether a selected execution-journal path matches jterm_core's bounded,
/// terminal-visible file-name boundary. Non-UTF-8 Unix names remain valid as
/// long as their raw bytes contain no ASCII controls.
pub(crate) fn is_valid_journal_path(path: &Path) -> bool {
    let Some(file_name) = path.file_name() else {
        return false;
    };
    if file_name
        .to_str()
        .is_some_and(|name| name.eq_ignore_ascii_case(JOURNAL_LOCK_FILE_NAME))
    {
        return false;
    }
    let bytes = path.as_os_str().as_bytes();
    bytes.len() <= MAX_JOURNAL_PATH_BYTES
        && !bytes.iter().any(|byte| matches!(*byte, 0..=0x1f | 0x7f))
        && !path.to_str().is_some_and(|text| {
            text.chars()
                .any(crate::terminal_text::is_terminal_ambiguous)
        })
}

fn env_value_is_false(value: &str) -> bool {
    matches!(
        value.trim().to_ascii_lowercase().as_str(),
        "" | "0" | "off" | "false" | "no"
    )
}

pub fn unix_time_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|duration| duration.as_millis().min(u128::from(u64::MAX)) as u64)
        .unwrap_or(0)
}

/// Generate a compact, OSC-safe correlation ID without adding a UUID
/// dependency. Timestamp, process, session hash, and per-shell sequence make
/// collisions impractical while the value remains non-secret.
pub fn execution_id(session_id: Option<&str>, seq: u64) -> String {
    let mut hash = 0xcbf29ce484222325_u64;
    for byte in session_id.unwrap_or("").bytes() {
        hash ^= u64::from(byte);
        hash = hash.wrapping_mul(0x100000001b3);
    }
    format!(
        "jsh-{hash:016x}-{:x}-{:x}-{seq:x}",
        std::process::id(),
        unix_time_ms()
    )
}

fn validate_execution_id(id: &str) -> io::Result<&str> {
    if is_valid_execution_id(id) {
        Ok(id)
    } else {
        Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "invalid execution ID",
        ))
    }
}

/// Exact correlation-key grammar shared by OSC lifecycle metadata and JSONL.
pub(crate) fn is_valid_execution_id(id: &str) -> bool {
    !id.is_empty()
        && id.len() <= MAX_EXECUTION_ID_BYTES
        && id
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'_' | b'.'))
}

fn validate_session_id(id: &str) -> io::Result<&str> {
    if is_valid_session_id(id) {
        Ok(id)
    } else {
        Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "invalid session ID",
        ))
    }
}

/// Exact grammar shared by execution-journal records and OSC 7770 emission.
///
/// Session identifiers are correlation keys, so callers must reject invalid
/// values rather than truncate or escape them into a different valid key.
pub(crate) fn is_valid_session_id(id: &str) -> bool {
    !id.is_empty()
        && id.len() <= MAX_SESSION_ID_BYTES
        && id
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'_'))
}

/// Whether a cwd can be shared exactly with terminal and journal consumers.
///
/// A cwd has no truncation marker in schema v1. Silently shortening it would
/// identify a different directory, while controls or invisible formatting
/// could disguise terminal chrome. Callers therefore omit the optional OSC
/// field or reject the journal event unless the exact value passes.
pub(crate) fn is_valid_cwd(cwd: &str) -> bool {
    !cwd.is_empty()
        && cwd.len() <= MAX_CWD_BYTES
        && !cwd.chars().any(crate::terminal_text::is_terminal_ambiguous)
}

fn validate_cwd(cwd: &str) -> io::Result<&str> {
    is_valid_cwd(cwd).then_some(cwd).ok_or_else(|| {
        io::Error::new(
            io::ErrorKind::InvalidInput,
            "cwd must be exact, bounded, and visually unambiguous",
        )
    })
}

/// Whether command text is exact, bounded, and safe to carry as reviewed
/// metadata. Newline and tab remain structural shell syntax; other controls
/// and invisible formatting cannot enter a terminal or journal display path.
pub(crate) fn is_valid_command_text(command: &str, max_bytes: usize) -> bool {
    !command.is_empty()
        && command.len() <= max_bytes
        && !command.chars().any(|ch| {
            (ch.is_control() && !matches!(ch, '\n' | '\t'))
                || (!ch.is_control() && crate::terminal_text::is_terminal_ambiguous(ch))
        })
}

fn validate_command_text(command: &str, max_bytes: usize) -> io::Result<&str> {
    is_valid_command_text(command, max_bytes)
        .then_some(command)
        .ok_or_else(|| {
            io::Error::new(
                io::ErrorKind::InvalidInput,
                "command text must be exact, bounded, and visually unambiguous",
            )
        })
}

fn normalize_output_metadata(
    retained_bytes: usize,
    truncated: bool,
    total_bytes: u64,
) -> (bool, u64) {
    let retained_bytes = u64::try_from(retained_bytes).unwrap_or(u64::MAX);
    let total_bytes = total_bytes.max(retained_bytes);
    (truncated || total_bytes > retained_bytes, total_bytes)
}

fn bounded_text(value: &str, max_bytes: usize) -> (String, bool) {
    if value.len() <= max_bytes {
        return (value.to_string(), false);
    }
    let head_budget = max_bytes / 2;
    let tail_budget = max_bytes - head_budget;
    let mut head_end = head_budget;
    while !value.is_char_boundary(head_end) {
        head_end -= 1;
    }
    let mut tail_start = value.len() - tail_budget;
    while !value.is_char_boundary(tail_start) {
        tail_start += 1;
    }
    let mut result = String::with_capacity(max_bytes);
    result.push_str(&value[..head_end]);
    result.push_str(&value[tail_start..]);
    (result, true)
}

fn read_records(path: &Path) -> io::Result<Vec<FoldedExecution>> {
    read_records_with_line_limit(path, MAX_JOURNAL_EVENT_LINES)
}

fn read_records_with_line_limit(
    path: &Path,
    max_event_lines: usize,
) -> io::Result<Vec<FoldedExecution>> {
    let file = open_regular_file(path, true, false, false)?;
    set_private_open_file_permissions(&file)?;
    if file.metadata()?.len() > MAX_JOURNAL_READ_BYTES {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "execution journal exceeds size limit",
        ));
    }
    let mut records = HashMap::<String, FoldedExecution>::new();
    // Keep the working set bounded while preserving the newest start-event
    // chronology. A second start for one ID is authoritative and must move to
    // its new position rather than retaining the first event's eviction age.
    // The ordered index keeps both replacement and eviction logarithmic under
    // a hostile stream of tiny duplicate or unique records.
    let mut record_order = BTreeMap::<(u64, u64, String), String>::new();
    let mut reader = BufReader::new(file);
    let mut line = Vec::new();
    let mut bytes_read = 0_u64;
    let mut event_lines = 0usize;
    while let Some(within_limit) = read_bounded_line(&mut reader, &mut line, &mut bytes_read)? {
        event_lines = event_lines.checked_add(1).ok_or_else(|| {
            io::Error::new(
                io::ErrorKind::InvalidData,
                "execution journal event count overflowed",
            )
        })?;
        if event_lines > max_event_lines {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "execution journal exceeds event-count limit",
            ));
        }
        if !within_limit {
            continue;
        }
        // A recognized v1 Start owns this ID even when strict decoding of its
        // remaining metadata fails. Retire the old lifecycle first so later
        // Finish/Output events cannot bind back to stale command/session data.
        if let Some(id) = recognized_v1_start_id(&line) {
            if let Some(previous) = records.remove(id.as_ref()) {
                record_order.remove(&(
                    previous.record.started_at_ms,
                    previous.record.seq,
                    previous.record.id,
                ));
            }
        }
        let Ok(event) = serde_json::from_slice::<ExecutionEvent>(&line) else {
            continue;
        };
        if event.version() != EXECUTION_JOURNAL_VERSION {
            continue;
        }
        match event {
            ExecutionEvent::Start(StartEvent {
                id,
                session_id,
                seq,
                command,
                command_truncated,
                cwd,
                started_at_ms,
                ..
            }) => {
                if validate_execution_id(&id).is_err()
                    || session_id
                        .as_deref()
                        .is_some_and(|id| validate_session_id(id).is_err())
                    || !is_valid_command_text(&command, MAX_COMMAND_BYTES)
                    || validate_cwd(&cwd).is_err()
                {
                    continue;
                }
                let record = ExecutionRecord {
                    id: id.clone(),
                    session_id,
                    seq,
                    command,
                    command_truncated,
                    cwd,
                    started_at_ms,
                    exit_code: None,
                    duration_ms: None,
                    cwd_after: None,
                    ended_at_ms: None,
                    output: None,
                };
                record_order.insert((started_at_ms, seq, id.clone()), id.clone());
                records.insert(id, FoldedExecution::new(record));
                while records.len() > MAX_RETAINED_EXECUTIONS {
                    let Some((_, oldest_id)) = record_order.pop_first() else {
                        break;
                    };
                    records.remove(&oldest_id);
                }
            }
            ExecutionEvent::Finish {
                id,
                exit_code,
                duration_ms,
                cwd_after,
                ended_at_ms,
                ..
            } => {
                if validate_execution_id(&id).is_err() || validate_cwd(&cwd_after).is_err() {
                    continue;
                }
                if let Some(record) = records.get_mut(&id) {
                    record.apply_finish(exit_code, duration_ms, cwd_after, ended_at_ms);
                }
            }
            ExecutionEvent::Output {
                id,
                text,
                truncated,
                total_bytes,
                captured_at_ms,
                ..
            } => {
                if validate_execution_id(&id).is_err() || text.len() > MAX_OUTPUT_BYTES {
                    continue;
                }
                if let Some(record) = records.get_mut(&id) {
                    let (truncated, total_bytes) =
                        normalize_output_metadata(text.len(), truncated, total_bytes);
                    record.apply_output(ExecutionOutput {
                        total_bytes,
                        text,
                        truncated,
                        captured_at_ms,
                    });
                }
            }
            ExecutionEvent::Conflict(event) => {
                if validate_execution_id(&event.id).is_err() {
                    continue;
                }
                if let Some(record) = records.get_mut(&event.id) {
                    match event.slot {
                        ConflictSlot::Finish => record.poison_finish(),
                        ConflictSlot::Output => record.poison_output(),
                    }
                }
            }
        }
    }
    let mut records = records.into_values().collect::<Vec<_>>();
    records.sort_by(|left, right| {
        (left.record.started_at_ms, left.record.seq, &left.record.id).cmp(&(
            right.record.started_at_ms,
            right.record.seq,
            &right.record.id,
        ))
    });
    Ok(records)
}

/// Read and, when necessary, discard one JSONL record without allocating more
/// than the public per-event limit. `false` denotes an oversized record.
fn read_bounded_line(
    reader: &mut impl BufRead,
    line: &mut Vec<u8>,
    bytes_read: &mut u64,
) -> io::Result<Option<bool>> {
    line.clear();
    let mut saw_bytes = false;
    let mut oversized = false;
    loop {
        let buffer = reader.fill_buf()?;
        if buffer.is_empty() {
            return Ok(saw_bytes.then_some(!oversized));
        }
        saw_bytes = true;
        let newline = buffer.iter().position(|byte| *byte == b'\n');
        let consumed = newline.map_or(buffer.len(), |index| index + 1);
        if !oversized {
            // The public limit covers the JSON event, not its optional line
            // delimiter.  Counting `+ 1` unconditionally admitted an
            // unterminated event with MAX_EVENT_LINE_BYTES + 1 payload bytes.
            // Discount a newline only when this chunk actually contains one.
            let payload_bytes = consumed.saturating_sub(usize::from(newline.is_some()));
            if line.len().saturating_add(payload_bytes) <= MAX_EVENT_LINE_BYTES {
                line.extend_from_slice(&buffer[..consumed]);
            } else {
                line.clear();
                oversized = true;
            }
        }
        reader.consume(consumed);
        *bytes_read = bytes_read.saturating_add(consumed as u64);
        if *bytes_read > MAX_JOURNAL_READ_BYTES {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "execution journal grew beyond size limit while being read",
            ));
        }
        if newline.is_some() {
            return Ok(Some(!oversized));
        }
    }
}

fn compact_unlocked(path: &Path) -> io::Result<()> {
    compact_unlocked_with_line_limit(path, MAX_JOURNAL_EVENT_LINES)
}

fn compact_unlocked_with_line_limit(path: &Path, max_event_lines: usize) -> io::Result<()> {
    let records = read_records(path)?;
    let mut retained = Vec::<Vec<u8>>::new();
    let mut retained_bytes = 0usize;
    let mut retained_event_lines = 0usize;
    for record in records.iter().rev().take(MAX_RETAINED_EXECUTIONS) {
        let encoded = encode_compacted_record(record)?;
        if retained_bytes + encoded.len() > COMPACTED_JOURNAL_TARGET_BYTES {
            break;
        }
        retained_event_lines = retained_event_lines
            .checked_add(encoded.iter().filter(|byte| **byte == b'\n').count())
            .ok_or_else(|| {
                io::Error::new(
                    io::ErrorKind::InvalidData,
                    "compacted execution journal event count overflowed",
                )
            })?;
        if retained_event_lines > max_event_lines {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "compacted execution journal exceeds event-count limit",
            ));
        }
        retained_bytes += encoded.len();
        retained.push(encoded);
    }
    let counter = TEMP_FILE_COUNTER.fetch_add(1, Ordering::Relaxed);
    let tmp_path = path.with_extension(format!("tmp.{}.{}", std::process::id(), counter));
    let result = (|| {
        let mut file = OpenOptions::new()
            .write(true)
            .create_new(true)
            .custom_flags(nix::libc::O_NOFOLLOW | nix::libc::O_CLOEXEC)
            .mode(0o600)
            .open(&tmp_path)?;
        set_private_open_file_permissions(&file)?;
        for encoded in retained.iter().rev() {
            file.write_all(encoded)?;
        }
        file.sync_all()?;
        fs::rename(&tmp_path, path)?;
        let directory = ensure_journal_parent(path, false)?;
        directory.sync_all()
    })();
    if result.is_err() {
        let _ = fs::remove_file(&tmp_path);
    }
    result
}

fn encode_compacted_record(folded: &FoldedExecution) -> io::Result<Vec<u8>> {
    let mut encoded = Vec::new();
    let record = &folded.record;
    write_compacted_event(
        &mut encoded,
        &ExecutionEvent::Start(StartEvent {
            jsh_execution_version: EXECUTION_JOURNAL_VERSION,
            id: record.id.clone(),
            session_id: record.session_id.clone(),
            seq: record.seq,
            command: record.command.clone(),
            command_truncated: record.command_truncated,
            cwd: record.cwd.clone(),
            started_at_ms: record.started_at_ms,
        }),
    )?;
    if folded.finish_conflicted {
        write_compacted_event(
            &mut encoded,
            &ExecutionEvent::Conflict(ConflictEvent {
                jsh_execution_version: EXECUTION_JOURNAL_VERSION,
                id: record.id.clone(),
                slot: ConflictSlot::Finish,
            }),
        )?;
    } else if let (Some(exit_code), Some(duration_ms), Some(cwd_after), Some(ended_at_ms)) = (
        record.exit_code,
        record.duration_ms,
        record.cwd_after.clone(),
        record.ended_at_ms,
    ) {
        write_compacted_event(
            &mut encoded,
            &ExecutionEvent::Finish {
                jsh_execution_version: EXECUTION_JOURNAL_VERSION,
                id: record.id.clone(),
                exit_code,
                duration_ms,
                cwd_after,
                ended_at_ms,
            },
        )?;
    }
    if folded.output_conflicted {
        write_compacted_event(
            &mut encoded,
            &ExecutionEvent::Conflict(ConflictEvent {
                jsh_execution_version: EXECUTION_JOURNAL_VERSION,
                id: record.id.clone(),
                slot: ConflictSlot::Output,
            }),
        )?;
    } else if let Some(output) = &record.output {
        write_compacted_event(
            &mut encoded,
            &ExecutionEvent::Output {
                jsh_execution_version: EXECUTION_JOURNAL_VERSION,
                id: record.id.clone(),
                text: output.text.clone(),
                truncated: output.truncated,
                total_bytes: output.total_bytes,
                captured_at_ms: output.captured_at_ms,
            },
        )?;
    }
    Ok(encoded)
}

fn write_compacted_event(file: &mut impl Write, event: &ExecutionEvent) -> io::Result<()> {
    serde_json::to_writer(&mut *file, event).map_err(io::Error::other)?;
    file.write_all(b"\n")
}

#[cfg(test)]
mod tests {
    use super::*;
    use nix::sys::stat::Mode;
    use nix::unistd::mkfifo;
    use std::os::unix::fs::symlink;

    fn journal() -> (tempfile::TempDir, ExecutionJournal) {
        let dir = tempfile::tempdir().unwrap();
        fs::set_permissions(dir.path(), fs::Permissions::from_mode(0o700)).unwrap();
        let journal = ExecutionJournal::with_path(dir.path().join("executions.jsonl"));
        (dir, journal)
    }

    fn start_event_with_raw_bytes(id: &str, raw_bytes: usize) -> Vec<u8> {
        let prefix = format!(
            "{{\"jsh_execution_version\":1,\"event\":\"start\",\"id\":\"{id}\",\"session_id\":\"wanted\",\"seq\":9,\"command\":\""
        );
        let suffix = b"\",\"cwd\":\"/new\",\"started_at_ms\":9}";
        assert!(raw_bytes >= prefix.len() + suffix.len());
        let mut event = prefix.into_bytes();
        event.resize(raw_bytes - suffix.len(), b'x');
        event.extend_from_slice(suffix);
        assert_eq!(event.len(), raw_bytes);
        assert!(serde_json::from_slice::<serde_json::Value>(&event).is_ok());
        event
    }

    fn escaped_multibyte_output_event(id: &str, decoded_bytes: usize) -> Vec<u8> {
        let mut event = format!(
            "{{\"jsh_execution_version\":1,\"event\":\"output\",\"id\":\"{id}\",\"text\":\""
        )
        .into_bytes();
        for _ in 0..decoded_bytes / "界".len() {
            event.extend_from_slice(br"\u754c");
        }
        event.resize(event.len() + decoded_bytes % "界".len(), b'x');
        event.extend_from_slice(
            format!(
                "\",\"truncated\":false,\"total_bytes\":{decoded_bytes},\"captured_at_ms\":2}}"
            )
            .as_bytes(),
        );
        event
    }

    /// A journal written before the 0.2.0 rename tags every event with
    /// `rsh_execution_version`. Without the serde alias those events fail to
    /// deserialize and a migrated journal reads as empty, so `context
    /// last-failed` loses everything recorded under the old name.
    #[test]
    fn pre_rename_journal_events_are_still_readable() {
        let (_dir, journal) = journal();
        let mut file = OpenOptions::new()
            .create(true)
            .append(true)
            .mode(0o600)
            .open(journal.path())
            .unwrap();
        writeln!(file, "{{\"rsh_execution_version\":1,\"event\":\"start\",\"id\":\"rsh-a\",\"session_id\":\"tab-1\",\"seq\":3,\"command\":\"make\",\"cwd\":\"/p\",\"started_at_ms\":10}}").unwrap();
        writeln!(file, "{{\"rsh_execution_version\":1,\"event\":\"finish\",\"id\":\"rsh-a\",\"exit_code\":2,\"duration_ms\":5,\"cwd_after\":\"/p\",\"ended_at_ms\":15}}").unwrap();
        writeln!(file, "{{\"rsh_execution_version\":1,\"event\":\"output\",\"id\":\"rsh-a\",\"text\":\"boom\",\"truncated\":false,\"total_bytes\":4,\"captured_at_ms\":16}}").unwrap();
        drop(file);

        let records = journal.records().unwrap();
        assert_eq!(records.len(), 1);
        assert_eq!(records[0].id, "rsh-a");
        assert_eq!(records[0].seq, 3);
        assert_eq!(records[0].command, "make");
        assert_eq!(records[0].exit_code, Some(2));
        assert_eq!(records[0].output.as_ref().unwrap().text, "boom");
        assert_eq!(journal.last_failed().unwrap().unwrap().id, "rsh-a");
    }

    #[test]
    fn folds_start_finish_and_terminal_output() {
        let (_dir, journal) = journal();
        journal
            .record_start("jsh-a", Some("tab-1"), 7, "false", "/before", 10)
            .unwrap();
        journal.record_finish("jsh-a", 1, 25, "/after", 35).unwrap();
        journal
            .record_output("jsh-a", "real terminal error", false, 19, 36)
            .unwrap();

        let records = journal.records().unwrap();
        assert_eq!(records.len(), 1);
        assert_eq!(records[0].exit_code, Some(1));
        assert_eq!(records[0].cwd_after.as_deref(), Some("/after"));
        assert_eq!(
            records[0].output.as_ref().unwrap().text,
            "real terminal error"
        );
        assert_eq!(journal.last_failed().unwrap().unwrap().id, "jsh-a");
        assert_eq!(journal.show("jsh-a").unwrap().unwrap().seq, 7);
        assert_eq!(journal.list(Some("tab-1"), 1).unwrap().len(), 1);
        assert!(journal.list(Some("another-tab"), 10).unwrap().is_empty());
    }

    #[test]
    fn output_byte_evidence_cannot_deny_truncation() {
        let (_writer_dir, writer_journal) = journal();
        writer_journal
            .record_start("jsh-writer", None, 1, "true", "/tmp", 1)
            .unwrap();
        writer_journal
            .record_output("jsh-writer", "hi", false, 3, 2)
            .unwrap();

        let writer_output = writer_journal
            .get("jsh-writer")
            .unwrap()
            .unwrap()
            .output
            .unwrap();
        assert!(writer_output.truncated);
        assert_eq!(writer_output.total_bytes, 3);

        let (_reader_dir, reader_journal) = journal();
        let mut file = OpenOptions::new()
            .create(true)
            .append(true)
            .mode(0o600)
            .open(reader_journal.path())
            .unwrap();
        writeln!(file, "{{\"jsh_execution_version\":1,\"event\":\"start\",\"id\":\"jsh-reader\",\"session_id\":null,\"seq\":1,\"command\":\"true\",\"cwd\":\"/tmp\",\"started_at_ms\":1}}").unwrap();
        writeln!(file, "{{\"jsh_execution_version\":1,\"event\":\"output\",\"id\":\"jsh-reader\",\"text\":\"hi\",\"truncated\":false,\"total_bytes\":3,\"captured_at_ms\":2}}").unwrap();
        drop(file);

        let reader_output = reader_journal
            .get("jsh-reader")
            .unwrap()
            .unwrap()
            .output
            .unwrap();
        assert!(reader_output.truncated);
        assert_eq!(reader_output.total_bytes, 3);
    }

    #[test]
    fn a_later_duplicate_start_replaces_lifecycle_and_chronology() {
        let (_dir, journal) = journal();
        journal
            .record_start("jsh-reused", Some("old-tab"), 1, "old", "/old", 10)
            .unwrap();
        journal
            .record_finish("jsh-reused", 9, 5, "/old-after", 15)
            .unwrap();
        journal
            .record_start("jsh-middle", None, 2, "middle", "/middle", 20)
            .unwrap();
        journal
            .record_start("jsh-reused", Some("new-tab"), 3, "new", "/new", 30)
            .unwrap();

        let records = journal.records().unwrap();
        assert_eq!(
            records
                .iter()
                .map(|record| record.id.as_str())
                .collect::<Vec<_>>(),
            ["jsh-middle", "jsh-reused"]
        );
        let replacement = &records[1];
        assert_eq!(replacement.seq, 3);
        assert_eq!(replacement.session_id.as_deref(), Some("new-tab"));
        assert_eq!(replacement.command, "new");
        assert_eq!(replacement.cwd, "/new");
        assert_eq!(replacement.exit_code, None);
        assert!(journal.list(Some("old-tab"), 10).unwrap().is_empty());
    }

    #[test]
    fn conflicting_lifecycle_slots_do_not_resolve_last_wins() {
        let (_dir, journal) = journal();
        let mut file = OpenOptions::new()
            .create(true)
            .append(true)
            .mode(0o600)
            .open(journal.path())
            .unwrap();
        writeln!(file, "{{\"jsh_execution_version\":1,\"event\":\"start\",\"id\":\"jsh-conflict\",\"session_id\":\"tab-1\",\"seq\":1,\"command\":\"true\",\"cwd\":\"/tmp\",\"started_at_ms\":1}}").unwrap();
        for event in [
            "{\"jsh_execution_version\":1,\"event\":\"finish\",\"id\":\"jsh-conflict\",\"exit_code\":0,\"duration_ms\":2,\"cwd_after\":\"/tmp\",\"ended_at_ms\":3}",
            "{\"jsh_execution_version\":1,\"event\":\"finish\",\"id\":\"jsh-conflict\",\"exit_code\":0,\"duration_ms\":2,\"cwd_after\":\"/tmp\",\"ended_at_ms\":3}",
            "{\"jsh_execution_version\":1,\"event\":\"finish\",\"id\":\"jsh-conflict\",\"exit_code\":9,\"duration_ms\":8,\"cwd_after\":\"/other\",\"ended_at_ms\":9}",
            "{\"jsh_execution_version\":1,\"event\":\"finish\",\"id\":\"jsh-conflict\",\"exit_code\":0,\"duration_ms\":2,\"cwd_after\":\"/tmp\",\"ended_at_ms\":3}",
            "{\"jsh_execution_version\":1,\"event\":\"output\",\"id\":\"jsh-conflict\",\"text\":\"first\",\"truncated\":false,\"total_bytes\":5,\"captured_at_ms\":4}",
            "{\"jsh_execution_version\":1,\"event\":\"output\",\"id\":\"jsh-conflict\",\"text\":\"first\",\"truncated\":false,\"total_bytes\":5,\"captured_at_ms\":4}",
            "{\"jsh_execution_version\":1,\"event\":\"output\",\"id\":\"jsh-conflict\",\"text\":\"second\",\"truncated\":false,\"total_bytes\":6,\"captured_at_ms\":10}",
            "{\"jsh_execution_version\":1,\"event\":\"output\",\"id\":\"jsh-conflict\",\"text\":\"first\",\"truncated\":false,\"total_bytes\":5,\"captured_at_ms\":4}",
        ] {
            writeln!(file, "{event}").unwrap();
        }
        drop(file);

        let record = journal.get("jsh-conflict").unwrap().unwrap();
        assert_eq!(record.exit_code, None);
        assert_eq!(record.duration_ms, None);
        assert_eq!(record.cwd_after, None);
        assert_eq!(record.ended_at_ms, None);
        assert_eq!(record.output, None);

        let before_compaction = journal.records().unwrap();
        compact_unlocked(journal.path()).unwrap();
        let after_compaction = journal.records().unwrap();
        assert_eq!(after_compaction, before_compaction);

        let compacted = fs::read_to_string(journal.path()).unwrap();
        let compacted_lines = compacted.lines().collect::<Vec<_>>();
        assert_eq!(compacted_lines.len(), 3);
        assert_eq!(
            serde_json::from_str::<serde_json::Value>(compacted_lines[1]).unwrap(),
            serde_json::json!({
                "event": "conflict",
                "jsh_execution_version": 1,
                "id": "jsh-conflict",
                "slot": "finish",
            })
        );
        assert_eq!(
            serde_json::from_str::<serde_json::Value>(compacted_lines[2]).unwrap(),
            serde_json::json!({
                "event": "conflict",
                "jsh_execution_version": 1,
                "id": "jsh-conflict",
                "slot": "output",
            })
        );

        let mut file = OpenOptions::new()
            .append(true)
            .open(journal.path())
            .unwrap();
        writeln!(file, "{}", compacted_lines[1]).unwrap();
        writeln!(file, "{}", compacted_lines[2]).unwrap();
        drop(file);
        journal
            .record_finish("jsh-conflict", 0, 2, "/tmp", 3)
            .unwrap();
        journal
            .record_output("jsh-conflict", "first", false, 5, 4)
            .unwrap();
        assert_eq!(journal.records().unwrap(), before_compaction);

        journal
            .record_start("jsh-conflict", Some("tab-1"), 2, "fresh", "/tmp", 20)
            .unwrap();
        let mut file = OpenOptions::new()
            .append(true)
            .open(journal.path())
            .unwrap();
        for malformed in [
            "{\"jsh_execution_version\":1,\"event\":\"conflict\",\"id\":\"jsh-conflict\",\"slot\":\"finish\",\"extra\":true}",
            "{\"jsh_execution_version\":1,\"event\":\"conflict\",\"id\":\"jsh-conflict\",\"slot\":\"unknown\"}",
            "{\"jsh_execution_version\":1,\"event\":\"conflict\",\"id\":\"jsh-conflict\"}",
            "{\"jsh_execution_version\":1,\"event\":\"conflicts\",\"id\":\"jsh-conflict\",\"slot\":\"finish\"}",
            "{\"jsh_execution_version\":99,\"event\":\"conflict\",\"id\":\"jsh-conflict\",\"slot\":\"finish\"}",
            "{\"jsh_execution_version\":1,\"event\":\"conflict\",\"id\":\"bad id\",\"slot\":\"finish\"}",
            "{\"jsh_execution_version\":1,\"event\":\"conflict\",\"id\":\"orphan\",\"slot\":\"finish\"}",
        ] {
            writeln!(file, "{malformed}").unwrap();
        }
        drop(file);
        journal
            .record_finish("jsh-conflict", 7, 2, "/after", 22)
            .unwrap();
        journal
            .record_output("jsh-conflict", "exact", false, 5, 23)
            .unwrap();

        let reset = journal.get("jsh-conflict").unwrap().unwrap();
        assert_eq!(reset.seq, 2);
        assert_eq!(reset.command, "fresh");
        assert_eq!(reset.exit_code, Some(7));
        assert_eq!(reset.duration_ms, Some(2));
        assert_eq!(reset.cwd_after.as_deref(), Some("/after"));
        assert_eq!(reset.ended_at_ms, Some(22));
        assert_eq!(
            reset.output.as_ref().map(|output| output.text.as_str()),
            Some("exact")
        );
    }

    #[test]
    fn recognized_start_ids_barrier_invalid_replacement_lifecycles() {
        let (_dir, journal) = journal();
        let mut file = OpenOptions::new()
            .create(true)
            .append(true)
            .mode(0o600)
            .open(journal.path())
            .unwrap();
        let valid_start = |id: &str, seq: u64| {
            serde_json::json!({
                "jsh_execution_version": 1,
                "event": "start",
                "id": id,
                "session_id": "wanted",
                "seq": seq,
                "command": "old",
                "cwd": "/old",
                "started_at_ms": seq,
            })
        };
        let finish = |id: &str| {
            serde_json::json!({
                "jsh_execution_version": 1,
                "event": "finish",
                "id": id,
                "exit_code": 9,
                "duration_ms": 1,
                "cwd_after": "/after",
                "ended_at_ms": 99,
            })
        };
        let mut replacements = vec![
            (
                "bad-session",
                serde_json::json!({
                    "jsh_execution_version": 1,
                    "event": "start",
                    "id": "bad-session",
                    "session_id": "bad session",
                    "seq": 10,
                    "command": "new",
                    "cwd": "/new",
                    "started_at_ms": 10,
                }),
            ),
            (
                "bad-command",
                serde_json::json!({
                    "jsh_execution_version": 1,
                    "event": "start",
                    "id": "bad-command",
                    "session_id": "wanted",
                    "seq": 11,
                    "command": "hidden\u{202e}command",
                    "cwd": "/new",
                    "started_at_ms": 11,
                }),
            ),
            (
                "bad-cwd",
                serde_json::json!({
                    "jsh_execution_version": 1,
                    "event": "start",
                    "id": "bad-cwd",
                    "session_id": "wanted",
                    "seq": 12,
                    "command": "new",
                    "cwd": "",
                    "started_at_ms": 12,
                }),
            ),
            (
                "bad-type",
                serde_json::json!({
                    "jsh_execution_version": 1,
                    "event": "start",
                    "id": "bad-type",
                    "session_id": "wanted",
                    "seq": "13",
                    "command": "new",
                    "cwd": "/new",
                    "started_at_ms": 13,
                }),
            ),
            (
                "extra-field",
                serde_json::json!({
                    "jsh_execution_version": 1,
                    "event": "start",
                    "id": "extra-field",
                    "session_id": "wanted",
                    "seq": 14,
                    "command": "new",
                    "cwd": "/new",
                    "started_at_ms": 14,
                    "extra": true,
                }),
            ),
            (
                "legacy-barrier",
                serde_json::json!({
                    "rsh_execution_version": 1,
                    "event": "start",
                    "id": "legacy-barrier",
                    "session_id": "wanted",
                    "seq": 15,
                    "command": "new",
                    "cwd": "",
                    "started_at_ms": 15,
                }),
            ),
        ];
        replacements.push((
            "oversized-command",
            serde_json::json!({
                "jsh_execution_version": 1,
                "event": "start",
                "id": "oversized-command",
                "session_id": "wanted",
                "seq": 16,
                "command": "x".repeat(MAX_COMMAND_BYTES + 1),
                "cwd": "/new",
                "started_at_ms": 16,
            }),
        ));

        for (index, (id, replacement)) in replacements.iter().enumerate() {
            writeln!(file, "{}", valid_start(id, index as u64 + 1)).unwrap();
            writeln!(file, "{replacement}").unwrap();
            writeln!(file, "{}", finish(id)).unwrap();
        }
        writeln!(file, "{}", valid_start("escaped-id", 17)).unwrap();
        writeln!(file, "{{\"jsh_execution_version\":1,\"event\":\"start\",\"id\":\"escaped\\u002did\",\"session_id\":\"wanted\",\"seq\":17,\"command\":\"new\",\"cwd\":\"\",\"started_at_ms\":17}}").unwrap();
        writeln!(file, "{}", finish("escaped-id")).unwrap();

        writeln!(file, "{}", valid_start("survivor", 20)).unwrap();
        for ignored in [
            "{\"jsh_execution_version\":1,\"event\":\"start\",\"session_id\":\"wanted\",\"seq\":21,\"command\":\"missing id\",\"cwd\":\"/new\",\"started_at_ms\":21}",
            "{\"jsh_execution_version\":1,\"event\":\"start\",\"id\":7,\"session_id\":\"wanted\",\"seq\":22,\"command\":\"wrong id type\",\"cwd\":\"/new\",\"started_at_ms\":22}",
            "{\"jsh_execution_version\":1,\"event\":\"start\",\"id\":\"survivor \" ,\"session_id\":\"wanted\",\"seq\":22,\"command\":\"invalid id\",\"cwd\":\"/new\",\"started_at_ms\":22}",
            "{\"jsh_execution_version\":1,\"event\":\"start\",\"id\":\"survivor\"",
            "{\"jsh_execution_version\":99,\"event\":\"start\",\"id\":\"survivor\",\"session_id\":\"wanted\",\"seq\":23,\"command\":\"future\",\"cwd\":\"/new\",\"started_at_ms\":23}",
            "{\"jsh_execution_version\":1,\"event\":\"future-start\",\"id\":\"survivor\",\"session_id\":\"wanted\",\"seq\":24,\"command\":\"extension\",\"cwd\":\"/new\",\"started_at_ms\":24}",
        ] {
            writeln!(file, "{ignored}").unwrap();
        }
        writeln!(file, "{}", finish("survivor")).unwrap();
        drop(file);

        let before_compaction = journal.records().unwrap();
        assert_eq!(before_compaction.len(), 1);
        assert_eq!(before_compaction[0].id, "survivor");
        assert_eq!(before_compaction[0].exit_code, Some(9));

        compact_unlocked(journal.path()).unwrap();
        assert_eq!(journal.records().unwrap(), before_compaction);
        let compacted = fs::read_to_string(journal.path()).unwrap();
        for (id, _) in &replacements {
            assert!(
                !compacted.contains(id),
                "stale id survived compaction: {id}"
            );
        }
        assert!(!compacted.contains("escaped-id"));

        journal
            .record_start("bad-cwd", Some("wanted"), 30, "fresh", "/fresh", 30)
            .unwrap();
        journal
            .record_finish("bad-cwd", 0, 1, "/fresh", 31)
            .unwrap();
        let revived = journal.records().unwrap();
        assert_eq!(
            revived
                .iter()
                .map(|record| record.id.as_str())
                .collect::<Vec<_>>(),
            ["survivor", "bad-cwd"]
        );
        assert_eq!(revived[1].command, "fresh");
    }

    /// Golden event emitted by `jterm_core::execution_journal`. Keep this
    /// independent of jsh's private `ExecutionEvent` serializer so a field or
    /// tag drift on either side breaks an explicit interoperability test.
    #[test]
    fn folds_jterm_core_v1_output_event_fixture() {
        let (_dir, journal) = journal();
        journal
            .record_start(
                "jsh-core-fixture",
                Some("tab-1"),
                9,
                "printf one\nprintf two",
                "/tmp",
                10,
            )
            .unwrap();
        let mut file = OpenOptions::new()
            .append(true)
            .open(journal.path())
            .unwrap();
        writeln!(file, "{{\"jsh_execution_version\":1,\"event\":\"output\",\"id\":\"jsh-core-fixture\",\"text\":\"one\\ntwo\",\"truncated\":false,\"total_bytes\":1,\"captured_at_ms\":12}}").unwrap();
        drop(file);

        let record = journal.get("jsh-core-fixture").unwrap().unwrap();
        assert_eq!(record.command, "printf one\nprintf two");
        let output = record.output.unwrap();
        assert_eq!(output.text, "one\ntwo");
        // Match jterm_core: a producer cannot claim fewer source bytes than
        // the exact UTF-8 payload retained in the event.
        assert_eq!(output.total_bytes, 7);
    }

    #[test]
    fn public_journal_contract_values_match_jterm_core_v1() {
        assert_eq!(EXECUTION_JOURNAL_VERSION, 1);
        assert_eq!(MAX_EVENT_LINE_BYTES, 1024 * 1024);
        assert_eq!(MAX_EXECUTION_ID_BYTES, 192);
        assert_eq!(MAX_SESSION_ID_BYTES, 128);
        assert_eq!(MAX_COMMAND_BYTES, 64 * 1024);
        assert_eq!(MAX_CWD_BYTES, 4 * 1024);
        assert_eq!(MAX_OUTPUT_BYTES, 256 * 1024);
        assert_eq!(MAX_JOURNAL_FILE_BYTES, 32 * 1024 * 1024);
        assert_eq!(MAX_JOURNAL_EVENT_LINES, 512 * 1024);
        assert_eq!(MAX_RETAINED_EXECUTIONS, 2_000);
    }

    #[test]
    fn malformed_unknown_and_orphan_events_are_ignored() {
        let (_dir, journal) = journal();
        journal
            .record_start("jsh-good", None, 1, "echo ok", "/tmp", 1)
            .unwrap();
        let mut file = OpenOptions::new()
            .append(true)
            .open(journal.path())
            .unwrap();
        writeln!(file, "not json").unwrap();
        writeln!(file, "{{\"jsh_execution_version\":99,\"event\":\"start\",\"id\":\"future\",\"session_id\":null,\"seq\":2,\"command\":\"x\",\"cwd\":\"/\",\"started_at_ms\":2}}").unwrap();
        writeln!(file, "{{\"jsh_execution_version\":1,\"event\":\"finish\",\"id\":\"orphan\",\"exit_code\":1,\"duration_ms\":1,\"cwd_after\":\"/\",\"ended_at_ms\":2}}").unwrap();
        assert_eq!(journal.records().unwrap().len(), 1);
    }

    #[test]
    fn append_after_a_torn_tail_preserves_the_new_event() {
        let (_dir, journal) = journal();
        fs::write(journal.path(), br#"{"event":"start"#).unwrap();
        fs::set_permissions(journal.path(), fs::Permissions::from_mode(0o600)).unwrap();

        journal
            .record_start("jsh-after-torn", None, 1, "echo recovered", "/tmp", 1)
            .unwrap();

        let records = journal.records().unwrap();
        assert_eq!(records.len(), 1);
        assert_eq!(records[0].id, "jsh-after-torn");
    }

    #[test]
    fn output_and_commands_are_hard_bounded_and_files_private() {
        let (dir, journal) = journal();
        let command = "x".repeat(MAX_COMMAND_BYTES + 100);
        let output = "e".repeat(MAX_OUTPUT_BYTES + 100);
        journal
            .record_start("jsh-bounded", None, 1, &command, "/tmp", 1)
            .unwrap();
        journal
            .record_output("jsh-bounded", &output, false, 0, 2)
            .unwrap();
        let record = journal.get("jsh-bounded").unwrap().unwrap();
        assert_eq!(record.command.len(), MAX_COMMAND_BYTES);
        assert!(record.command_truncated);
        assert_eq!(record.output.as_ref().unwrap().text.len(), MAX_OUTPUT_BYTES);
        assert!(record.output.as_ref().unwrap().truncated);
        assert_eq!(
            record.output.as_ref().unwrap().total_bytes,
            output.len() as u64
        );
        assert_eq!(
            fs::metadata(journal.path()).unwrap().permissions().mode() & 0o777,
            0o600
        );
        assert_eq!(
            fs::metadata(dir.path().join("executions.lock"))
                .unwrap()
                .permissions()
                .mode()
                & 0o777,
            0o600
        );
    }

    #[test]
    fn shared_writable_journal_and_lock_files_are_rejected_without_repair() {
        for target in ["journal", "lock"] {
            let (dir, journal) = journal();
            let path = if target == "journal" {
                journal.path().to_path_buf()
            } else {
                dir.path().join("executions.lock")
            };
            fs::write(&path, b"").unwrap();
            fs::set_permissions(&path, fs::Permissions::from_mode(0o622)).unwrap();

            assert!(
                journal
                    .record_start("jsh-shared-file", None, 1, "true", "/tmp", 1)
                    .is_err(),
                "target={target}"
            );
            assert_eq!(
                fs::metadata(&path).unwrap().permissions().mode() & 0o777,
                0o622,
                "target={target}"
            );
        }
    }

    #[test]
    fn owner_readable_journal_is_tightened_after_validation() {
        let (_dir, journal) = journal();
        fs::write(
            journal.path(),
            b"{\"jsh_execution_version\":1,\"event\":\"start\",\"id\":\"jsh-readable\",\"session_id\":null,\"seq\":1,\"command\":\"true\",\"cwd\":\"/tmp\",\"started_at_ms\":1}\n",
        )
        .unwrap();
        fs::set_permissions(journal.path(), fs::Permissions::from_mode(0o644)).unwrap();

        assert_eq!(journal.records().unwrap().len(), 1);
        assert_eq!(
            fs::metadata(journal.path()).unwrap().permissions().mode() & 0o777,
            0o600
        );
    }

    #[test]
    fn json_escape_shrinking_preserves_the_observed_output_size() {
        let (_dir, journal) = journal();
        journal
            .record_start("jsh-control-output", None, 1, "printf data", "/tmp", 1)
            .unwrap();
        let output = "\0".repeat(MAX_OUTPUT_BYTES);

        journal
            .record_output("jsh-control-output", &output, false, 0, 2)
            .unwrap();

        let record = journal.get("jsh-control-output").unwrap().unwrap();
        let captured = record.output.unwrap();
        assert!(captured.text.len() <= MAX_OUTPUT_BYTES / 2);
        assert!(captured.truncated);
        assert_eq!(captured.total_bytes, output.len() as u64);
        assert!(fs::read_to_string(journal.path())
            .unwrap()
            .lines()
            .all(|line| line.len() <= MAX_EVENT_LINE_BYTES));
    }

    #[test]
    fn cwd_values_are_exact_or_the_journal_event_is_rejected() {
        let (_dir, journal) = journal();
        let at_limit = "x".repeat(MAX_CWD_BYTES);
        journal
            .record_start("jsh-exact-cwd", None, 1, "true", &at_limit, 1)
            .unwrap();
        journal
            .record_finish("jsh-exact-cwd", 0, 1, "/after", 2)
            .unwrap();

        for invalid in [
            String::new(),
            "x".repeat(MAX_CWD_BYTES + 1),
            "/tmp/line\nbreak".to_string(),
            "/tmp/left\u{202e}right".to_string(),
        ] {
            assert!(journal
                .record_start("jsh-invalid-cwd", None, 2, "true", &invalid, 3)
                .is_err());
            assert!(journal
                .record_finish("jsh-exact-cwd", 0, 1, &invalid, 3)
                .is_err());
        }

        let records = journal.records().unwrap();
        assert_eq!(records.len(), 1);
        assert_eq!(records[0].cwd, at_limit);
        assert_eq!(records[0].cwd_after.as_deref(), Some("/after"));
    }

    #[test]
    fn command_values_preserve_structure_but_reject_display_ambiguity() {
        let (_dir, journal) = journal();
        journal
            .record_start(
                "jsh-structured-command",
                None,
                1,
                "printf one\nprintf '\t%s' two",
                "/tmp",
                1,
            )
            .unwrap();

        for invalid in ["", "echo\rhidden", "echo\x1b[2J", "left\u{202e}right"] {
            assert!(journal
                .record_start("jsh-invalid-command", None, 2, invalid, "/tmp", 2)
                .is_err());
        }

        let records = journal.records().unwrap();
        assert_eq!(records.len(), 1);
        assert_eq!(records[0].command, "printf one\nprintf '\t%s' two");
    }

    #[test]
    fn custom_existing_parent_permissions_are_not_changed() {
        let dir = tempfile::tempdir().unwrap();
        fs::set_permissions(dir.path(), fs::Permissions::from_mode(0o755)).unwrap();
        let journal = ExecutionJournal::with_path(dir.path().join("custom.jsonl"));
        journal
            .record_start("jsh-custom", None, 1, "true", "/tmp", 1)
            .unwrap();
        assert_eq!(
            fs::metadata(dir.path()).unwrap().permissions().mode() & 0o777,
            0o755
        );
    }

    #[test]
    fn an_explicit_default_path_remains_a_custom_namespace() {
        let default = default_journal_path().expect("state directory");
        let journal = ExecutionJournal::with_path(default.clone());

        assert!(!journal.harden_existing_parent);
        assert!(!journal_parent_hardening_for_override(Some(
            default.as_os_str()
        )));
        assert!(journal_parent_hardening_for_override(None));
        assert!(journal_parent_hardening_for_override(Some(
            std::ffi::OsStr::new("")
        )));
    }

    #[test]
    fn shared_writable_custom_parents_are_rejected() {
        let dir = tempfile::tempdir().unwrap();
        for (name, mode) in [("group", 0o770), ("sticky", 0o1777)] {
            let parent = dir.path().join(name);
            fs::create_dir(&parent).unwrap();
            fs::set_permissions(&parent, fs::Permissions::from_mode(mode)).unwrap();
            let journal = ExecutionJournal::with_path(parent.join("custom.jsonl"));

            assert!(
                journal
                    .record_start("jsh-custom", None, 1, "true", "/tmp", 1)
                    .is_err(),
                "mode={mode:o}"
            );
            assert!(!journal.path().exists(), "mode={mode:o}");
        }
    }

    #[test]
    fn a_default_parent_is_hardened_before_the_mode_gate() {
        let dir = tempfile::tempdir().unwrap();
        let parent = dir.path().join("default-state");
        fs::create_dir(&parent).unwrap();
        fs::set_permissions(&parent, fs::Permissions::from_mode(0o770)).unwrap();

        let held = ensure_journal_parent(&parent.join("executions.jsonl"), true).unwrap();
        drop(held);
        assert_eq!(
            fs::metadata(parent).unwrap().permissions().mode() & 0o777,
            0o700
        );
    }

    /// The directory rule that remains: a parent belonging to another account
    /// is refused. `/tmp` is root-owned and 1777 wherever it exists.
    #[test]
    fn a_custom_parent_owned_by_another_user_is_rejected() {
        if unsafe { nix::libc::geteuid() } == 0 {
            return; // root owns /tmp, so there is no third party to test against
        }
        let journal = ExecutionJournal::with_path(PathBuf::from("/tmp/.jsh-journal-probe.jsonl"));

        let error = journal
            .record_start("jsh-custom", None, 1, "true", "/tmp", 1)
            .expect_err("another user's directory accepted");

        assert_eq!(error.kind(), io::ErrorKind::PermissionDenied);
        assert!(!journal.path().exists());
    }

    #[test]
    fn newly_created_custom_parent_is_private() {
        let dir = tempfile::tempdir().unwrap();
        let parent = dir.path().join("new-jsh-state");
        let journal = ExecutionJournal::with_path(parent.join("custom.jsonl"));
        journal
            .record_start("jsh-custom", None, 1, "true", "/tmp", 1)
            .unwrap();
        assert_eq!(
            fs::metadata(parent).unwrap().permissions().mode() & 0o777,
            0o700
        );
    }

    #[test]
    fn start_rejects_unsafe_or_oversized_session_ids() {
        let (_dir, journal) = journal();
        assert!(journal
            .record_start("jsh-a", Some("bad;osc"), 1, "true", "/tmp", 1)
            .is_err());
        assert!(journal
            .record_start("jsh-b", Some(&"x".repeat(129)), 2, "true", "/tmp", 2)
            .is_err());
        assert!(journal.records().unwrap().is_empty());
    }

    #[test]
    fn execution_id_validation_matches_the_shared_protocol_grammar() {
        let generated = execution_id(Some("tab_1"), 7);
        for valid in [
            generated,
            "jsh-a_b.c-1".into(),
            "x".repeat(MAX_EXECUTION_ID_BYTES),
        ] {
            assert!(is_valid_execution_id(&valid), "id={valid:?}");
        }
        for invalid in [
            String::new(),
            "x".repeat(MAX_EXECUTION_ID_BYTES + 1),
            "jsh:1".into(),
            "has space".into(),
            "line\nbreak".into(),
            "雪".into(),
        ] {
            assert!(!is_valid_execution_id(&invalid), "id={invalid:?}");
        }
    }

    #[test]
    fn journal_path_override_must_be_absolute() {
        assert_eq!(
            select_journal_path(Some(std::ffi::OsString::new())),
            default_journal_path(),
            "an empty override must match jterm_core's unset/default semantics"
        );
        assert!(select_journal_path(Some("relative/file.jsonl".into())).is_none());
        assert_eq!(
            select_journal_path(Some("/tmp/jsh-test/executions.jsonl".into())),
            Some(PathBuf::from("/tmp/jsh-test/executions.jsonl"))
        );
    }

    #[test]
    fn journal_path_override_matches_the_terminal_path_boundary() {
        use std::os::unix::ffi::OsStringExt;

        assert_eq!(
            select_journal_path(Some("/tmp/executions.locked".into())),
            Some(PathBuf::from("/tmp/executions.locked"))
        );
        let maximum = format!("/{}", "x".repeat(MAX_JOURNAL_PATH_BYTES - 1));
        assert_eq!(
            select_journal_path(Some(maximum.clone().into())),
            Some(PathBuf::from(maximum))
        );
        assert!(select_journal_path(Some(
            format!("/{}", "x".repeat(MAX_JOURNAL_PATH_BYTES)).into()
        ))
        .is_none());

        for unsafe_path in [
            "/",
            "/tmp/bad\nname.jsonl",
            "/tmp/bad\u{0080}name.jsonl",
            "/tmp/bad\u{202e}name.jsonl",
            "/tmp/bad\u{fff9}name.jsonl",
            "/tmp/executions.lock",
            "/tmp/./executions.lock",
            "/tmp/EXECUTIONS.LOCK",
        ] {
            assert!(
                select_journal_path(Some(unsafe_path.into())).is_none(),
                "accepted {unsafe_path:?}"
            );
        }

        let non_utf8 = std::ffi::OsString::from_vec(b"/tmp/jsh-\xff.jsonl".to_vec());
        assert_eq!(
            select_journal_path(Some(non_utf8.clone())),
            Some(PathBuf::from(non_utf8))
        );
    }

    #[test]
    fn direct_journal_paths_cannot_alias_the_fixed_lock_sidecar() {
        let dir = tempfile::tempdir().unwrap();
        fs::set_permissions(dir.path(), fs::Permissions::from_mode(0o700)).unwrap();

        for name in ["executions.lock", "EXECUTIONS.LOCK"] {
            let path = dir.path().join(name);
            let journal = ExecutionJournal::with_path(path.clone());
            let error = journal
                .records()
                .expect_err("reserved lock sidecar alias was readable");
            assert_eq!(error.kind(), io::ErrorKind::InvalidInput, "name={name}");
            let error = journal
                .record_start("jsh-reserved", None, 1, "true", "/tmp", 1)
                .expect_err("reserved lock sidecar alias accepted");

            assert_eq!(error.kind(), io::ErrorKind::InvalidInput, "name={name}");
            assert!(!path.exists(), "created reserved alias {name:?}");
        }
    }

    #[test]
    fn oversized_line_is_discarded_without_hiding_the_next_event() {
        let (_dir, journal) = journal();
        let mut file = OpenOptions::new()
            .create(true)
            .append(true)
            .mode(0o600)
            .open(journal.path())
            .unwrap();
        for _ in 0..=MAX_EVENT_LINE_BYTES {
            file.write_all(b"x").unwrap();
        }
        file.write_all(b"\n").unwrap();
        write_compacted_event(
            &mut file,
            &ExecutionEvent::Start(StartEvent {
                jsh_execution_version: EXECUTION_JOURNAL_VERSION,
                id: "jsh-after-large-line".into(),
                session_id: None,
                seq: 1,
                command: "echo recovered".into(),
                command_truncated: false,
                cwd: "/tmp".into(),
                started_at_ms: 1,
            }),
        )
        .unwrap();
        drop(file);

        let records = journal.records().unwrap();
        assert_eq!(records.len(), 1);
        assert_eq!(records[0].id, "jsh-after-large-line");
    }

    #[test]
    fn raw_line_budget_precedes_start_barrier_semantics() {
        let (_dir, journal) = journal();
        let mut contents = concat!(
            "{\"jsh_execution_version\":1,\"event\":\"start\",\"id\":\"raw-exact\",\"session_id\":\"wanted\",\"seq\":1,\"command\":\"old\",\"cwd\":\"/old\",\"started_at_ms\":1}\n",
            "{\"jsh_execution_version\":1,\"event\":\"finish\",\"id\":\"raw-exact\",\"exit_code\":9,\"duration_ms\":1,\"cwd_after\":\"/old\",\"ended_at_ms\":2}\n",
            "{\"jsh_execution_version\":1,\"event\":\"start\",\"id\":\"raw-over\",\"session_id\":\"wanted\",\"seq\":2,\"command\":\"old\",\"cwd\":\"/old\",\"started_at_ms\":2}\n",
            "{\"jsh_execution_version\":1,\"event\":\"finish\",\"id\":\"raw-over\",\"exit_code\":9,\"duration_ms\":1,\"cwd_after\":\"/old\",\"ended_at_ms\":3}\n"
        )
        .as_bytes()
        .to_vec();
        contents.extend_from_slice(&start_event_with_raw_bytes(
            "raw-exact",
            MAX_EVENT_LINE_BYTES,
        ));
        contents.push(b'\n');
        contents.extend_from_slice(&start_event_with_raw_bytes(
            "raw-over",
            MAX_EVENT_LINE_BYTES + 1,
        ));
        contents.push(b'\n');
        fs::write(journal.path(), contents).unwrap();
        fs::set_permissions(journal.path(), fs::Permissions::from_mode(0o600)).unwrap();

        let records = journal.records().unwrap();

        assert_eq!(records.len(), 1);
        assert_eq!(records[0].id, "raw-over");
        assert_eq!(records[0].command, "old");
        assert_eq!(records[0].exit_code, Some(9));
    }

    #[test]
    fn decoded_utf8_budget_is_charged_after_json_unescaping() {
        let (_dir, journal) = journal();
        let mut contents = concat!(
            "{\"jsh_execution_version\":1,\"event\":\"start\",\"id\":\"decoded-exact\",\"session_id\":\"wanted\",\"seq\":1,\"command\":\"true\",\"cwd\":\"/\",\"started_at_ms\":1}\n",
            "{\"jsh_execution_version\":1,\"event\":\"start\",\"id\":\"decoded-over\",\"session_id\":\"wanted\",\"seq\":2,\"command\":\"true\",\"cwd\":\"/\",\"started_at_ms\":2}\n"
        )
        .as_bytes()
        .to_vec();
        contents.extend_from_slice(&escaped_multibyte_output_event(
            "decoded-exact",
            MAX_OUTPUT_BYTES,
        ));
        contents.push(b'\n');
        contents.extend_from_slice(&escaped_multibyte_output_event(
            "decoded-over",
            MAX_OUTPUT_BYTES + 1,
        ));
        contents.push(b'\n');
        fs::write(journal.path(), contents).unwrap();
        fs::set_permissions(journal.path(), fs::Permissions::from_mode(0o600)).unwrap();

        let records = journal.records().unwrap();

        assert_eq!(records.len(), 2);
        assert_eq!(records[0].id, "decoded-exact");
        assert_eq!(
            records[0].output.as_ref().unwrap().text.len(),
            MAX_OUTPUT_BYTES
        );
        assert_eq!(records[1].id, "decoded-over");
        assert_eq!(records[1].output, None);

        compact_unlocked(journal.path()).unwrap();
        let compacted = fs::read_to_string(journal.path()).unwrap();
        assert!(compacted
            .lines()
            .all(|line| line.len() <= MAX_EVENT_LINE_BYTES));
        assert_eq!(journal.records().unwrap(), records);
    }

    #[test]
    fn reader_rejects_excess_event_lines_before_parsing_them() {
        let (_dir, journal) = journal();
        let accepted = concat!(
            "{\"jsh_execution_version\":1,\"event\":\"start\",\"id\":\"bounded\",\"session_id\":\"wanted\",\"seq\":1,\"command\":\"true\",\"cwd\":\"/\",\"started_at_ms\":1}\n",
            "{\"jsh_execution_version\":1,\"event\":\"finish\",\"id\":\"bounded\",\"exit_code\":0,\"duration_ms\":1,\"cwd_after\":\"/\",\"ended_at_ms\":2}\n"
        );
        fs::write(journal.path(), accepted).unwrap();
        fs::set_permissions(journal.path(), fs::Permissions::from_mode(0o600)).unwrap();
        let records = read_records_with_line_limit(journal.path(), 2).unwrap();
        assert_eq!(records.len(), 1);
        assert_eq!(records[0].record.exit_code, Some(0));

        let over_limit = format!(
            "{accepted}{{\"jsh_execution_version\":1,\"event\":\"start\",\"id\":\"bounded\",\"unexpected\":true}}\n"
        );
        fs::write(journal.path(), over_limit).unwrap();
        let error = read_records_with_line_limit(journal.path(), 2).unwrap_err();

        assert_eq!(error.kind(), io::ErrorKind::InvalidData);
        assert!(error.to_string().contains("event-count limit"));
    }

    #[test]
    fn compaction_refuses_to_emit_more_events_than_readers_accept() {
        let (_dir, journal) = journal();
        let source = concat!(
            "{\"jsh_execution_version\":1,\"event\":\"start\",\"id\":\"bounded\",\"session_id\":\"wanted\",\"seq\":1,\"command\":\"true\",\"cwd\":\"/\",\"started_at_ms\":1}\n",
            "{\"jsh_execution_version\":1,\"event\":\"finish\",\"id\":\"bounded\",\"exit_code\":0,\"duration_ms\":1,\"cwd_after\":\"/\",\"ended_at_ms\":2}\n",
            "{\"jsh_execution_version\":1,\"event\":\"output\",\"id\":\"bounded\",\"text\":\"ok\",\"truncated\":false,\"total_bytes\":2,\"captured_at_ms\":3}\n"
        );
        fs::write(journal.path(), source).unwrap();
        fs::set_permissions(journal.path(), fs::Permissions::from_mode(0o600)).unwrap();

        let error = compact_unlocked_with_line_limit(journal.path(), 2).unwrap_err();
        assert_eq!(error.kind(), io::ErrorKind::InvalidData);
        assert!(error.to_string().contains("event-count limit"));
        assert_eq!(fs::read_to_string(journal.path()).unwrap(), source);

        compact_unlocked(journal.path()).unwrap();
        assert_eq!(
            fs::read_to_string(journal.path()).unwrap().lines().count(),
            3
        );
        assert_eq!(journal.records().unwrap().len(), 1);
    }

    #[test]
    fn event_line_limit_counts_only_a_real_newline_as_framing() {
        for terminated in [false, true] {
            let mut bytes = vec![b'x'; MAX_EVENT_LINE_BYTES];
            if terminated {
                bytes.push(b'\n');
            }
            let mut reader = bytes.as_slice();
            let mut line = Vec::new();
            let mut bytes_read = 0;
            assert_eq!(
                read_bounded_line(&mut reader, &mut line, &mut bytes_read).unwrap(),
                Some(true),
                "an event exactly at the payload limit must be retained"
            );
            assert_eq!(line, bytes);
        }

        for terminated in [false, true] {
            let mut bytes = vec![b'x'; MAX_EVENT_LINE_BYTES + 1];
            if terminated {
                bytes.push(b'\n');
            }
            let mut reader = bytes.as_slice();
            let mut line = Vec::new();
            let mut bytes_read = 0;
            assert_eq!(
                read_bounded_line(&mut reader, &mut line, &mut bytes_read).unwrap(),
                Some(false),
                "one byte beyond the payload limit must be discarded"
            );
            assert!(line.is_empty());
        }
    }

    #[test]
    fn compaction_keeps_the_most_recent_execution_records() {
        let (_dir, journal) = journal();
        let mut file = OpenOptions::new()
            .create(true)
            .write(true)
            .truncate(true)
            .mode(0o600)
            .open(journal.path())
            .unwrap();
        for seq in 0..(MAX_RETAINED_EXECUTIONS as u64 + 5) {
            write_compacted_event(
                &mut file,
                &ExecutionEvent::Start(StartEvent {
                    jsh_execution_version: EXECUTION_JOURNAL_VERSION,
                    id: format!("jsh-{seq}"),
                    session_id: Some("tab".into()),
                    seq,
                    command: format!("echo {seq}"),
                    command_truncated: false,
                    cwd: "/tmp".into(),
                    started_at_ms: seq,
                }),
            )
            .unwrap();
        }
        drop(file);

        compact_unlocked(journal.path()).unwrap();
        let records = journal.records().unwrap();
        assert_eq!(records.len(), MAX_RETAINED_EXECUTIONS);
        assert_eq!(records.first().unwrap().seq, 5);
        assert_eq!(
            records.last().unwrap().seq,
            MAX_RETAINED_EXECUTIONS as u64 + 4
        );
    }

    #[test]
    fn compaction_also_enforces_the_byte_target() {
        let (_dir, journal) = journal();
        let output = "x".repeat(MAX_OUTPUT_BYTES);
        let mut file = OpenOptions::new()
            .create(true)
            .write(true)
            .truncate(true)
            .mode(0o600)
            .open(journal.path())
            .unwrap();
        for seq in 0..100_u64 {
            let id = format!("jsh-large-{seq}");
            write_compacted_event(
                &mut file,
                &ExecutionEvent::Start(StartEvent {
                    jsh_execution_version: EXECUTION_JOURNAL_VERSION,
                    id: id.clone(),
                    session_id: Some("tab".into()),
                    seq,
                    command: "failing-command".into(),
                    command_truncated: false,
                    cwd: "/tmp".into(),
                    started_at_ms: seq,
                }),
            )
            .unwrap();
            write_compacted_event(
                &mut file,
                &ExecutionEvent::Output {
                    jsh_execution_version: EXECUTION_JOURNAL_VERSION,
                    id,
                    text: output.clone(),
                    truncated: false,
                    total_bytes: MAX_OUTPUT_BYTES as u64,
                    captured_at_ms: seq,
                },
            )
            .unwrap();
        }
        drop(file);
        assert!(
            fs::metadata(journal.path()).unwrap().len() > COMPACTED_JOURNAL_TARGET_BYTES as u64
        );

        compact_unlocked(journal.path()).unwrap();
        assert!(
            fs::metadata(journal.path()).unwrap().len() <= COMPACTED_JOURNAL_TARGET_BYTES as u64
        );
        let records = journal.records().unwrap();
        assert!(!records.is_empty());
        assert!(records.len() < 100);
        assert_eq!(records.last().unwrap().seq, 99);
    }

    #[test]
    fn journal_symlinks_hard_links_and_fifos_are_rejected_without_touching_targets() {
        let dir = tempfile::tempdir().unwrap();
        let victim = dir.path().join("victim");
        fs::write(&victim, "keep me\n").unwrap();
        fs::set_permissions(&victim, fs::Permissions::from_mode(0o640)).unwrap();

        let symlink_path = dir.path().join("symlink.jsonl");
        symlink(&victim, &symlink_path).unwrap();
        let symlink_journal = ExecutionJournal::with_path(symlink_path);
        assert!(symlink_journal
            .record_start("jsh-link", None, 1, "true", "/tmp", 1)
            .is_err());

        let hardlink_path = dir.path().join("hardlink.jsonl");
        fs::hard_link(&victim, &hardlink_path).unwrap();
        let hardlink_journal = ExecutionJournal::with_path(hardlink_path);
        assert!(hardlink_journal.records().is_err());

        let fifo_path = dir.path().join("fifo.jsonl");
        mkfifo(&fifo_path, Mode::S_IRUSR | Mode::S_IWUSR).unwrap();
        let fifo_journal = ExecutionJournal::with_path(fifo_path);
        assert!(fifo_journal.records().is_err());
        assert!(fifo_journal
            .record_start("jsh-fifo", None, 1, "true", "/tmp", 1)
            .is_err());

        assert_eq!(fs::read_to_string(&victim).unwrap(), "keep me\n");
        assert_eq!(
            fs::metadata(&victim).unwrap().permissions().mode() & 0o777,
            0o640
        );
    }

    #[test]
    fn journal_lock_special_files_never_block_or_change_a_target() {
        for kind in ["symlink", "hardlink", "fifo"] {
            let dir = tempfile::tempdir().unwrap();
            let victim = dir.path().join("victim");
            let journal = ExecutionJournal::with_path(dir.path().join("events.jsonl"));
            fs::write(&victim, "keep lock\n").unwrap();
            fs::set_permissions(&victim, fs::Permissions::from_mode(0o640)).unwrap();
            match kind {
                "symlink" => symlink(&victim, &journal.lock_path).unwrap(),
                "hardlink" => fs::hard_link(&victim, &journal.lock_path).unwrap(),
                "fifo" => {
                    fs::remove_file(&victim).unwrap();
                    mkfifo(&journal.lock_path, Mode::S_IRUSR | Mode::S_IWUSR).unwrap();
                }
                _ => unreachable!(),
            }

            assert!(journal
                .record_start("jsh-lock", None, 1, "true", "/tmp", 1)
                .is_err());
            assert!(!journal.path().exists());
            if kind != "fifo" {
                assert_eq!(fs::read_to_string(&victim).unwrap(), "keep lock\n");
                assert_eq!(
                    fs::metadata(&victim).unwrap().permissions().mode() & 0o777,
                    0o640
                );
            }
        }
    }

    #[test]
    fn journal_parent_symlink_is_rejected_without_writing_through_it() {
        let dir = tempfile::tempdir().unwrap();
        let real_parent = dir.path().join("real");
        let linked_parent = dir.path().join("linked");
        fs::create_dir(&real_parent).unwrap();
        fs::set_permissions(&real_parent, fs::Permissions::from_mode(0o750)).unwrap();
        symlink(&real_parent, &linked_parent).unwrap();
        let journal = ExecutionJournal::with_path(linked_parent.join("events.jsonl"));

        assert!(journal
            .record_start("jsh-parent", None, 1, "true", "/tmp", 1)
            .is_err());
        assert!(!real_parent.join("events.jsonl").exists());
        assert!(!real_parent.join("executions.lock").exists());
        assert_eq!(
            fs::metadata(&real_parent).unwrap().permissions().mode() & 0o777,
            0o750
        );
    }

    #[test]
    fn journal_reader_rejects_a_file_beyond_the_recovery_budget() {
        let (_dir, journal) = journal();
        let file = OpenOptions::new()
            .write(true)
            .create_new(true)
            .mode(0o600)
            .open(journal.path())
            .unwrap();
        file.set_len(MAX_JOURNAL_READ_BYTES + 1).unwrap();
        drop(file);

        assert!(journal.records().is_err());
        assert!(journal
            .record_start("jsh-too-large", None, 1, "true", "/tmp", 1)
            .is_err());
        assert_eq!(
            fs::metadata(journal.path()).unwrap().len(),
            MAX_JOURNAL_READ_BYTES + 1
        );
    }

    #[test]
    fn decoder_retains_only_the_newest_bounded_execution_set() {
        let (_dir, journal) = journal();
        let mut file = OpenOptions::new()
            .write(true)
            .create_new(true)
            .mode(0o600)
            .open(journal.path())
            .unwrap();
        for seq in 0..(MAX_RETAINED_EXECUTIONS as u64 + 5) {
            write_compacted_event(
                &mut file,
                &ExecutionEvent::Start(StartEvent {
                    jsh_execution_version: EXECUTION_JOURNAL_VERSION,
                    id: format!("jsh-bounded-{seq}"),
                    session_id: None,
                    seq,
                    command: "true".into(),
                    command_truncated: false,
                    cwd: "/tmp".into(),
                    started_at_ms: seq,
                }),
            )
            .unwrap();
        }
        drop(file);

        let records = journal.records().unwrap();
        assert_eq!(records.len(), MAX_RETAINED_EXECUTIONS);
        assert_eq!(records.first().unwrap().seq, 5);
        assert_eq!(
            records.last().unwrap().seq,
            MAX_RETAINED_EXECUTIONS as u64 + 4
        );
    }
}
