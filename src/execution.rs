//! Structured command execution journal shared with terminal emulators.
//!
//! The journal is deliberately separate from command history. It is an
//! append-only JSONL event stream so jsh and a terminal can safely contribute
//! metadata without redirecting a child's stdout/stderr away from its PTY.

use nix::fcntl::{Flock, FlockArg};
use serde::{Deserialize, Serialize};
use std::collections::{BTreeMap, HashMap};
use std::fs::{self, File, OpenOptions};
use std::io::{self, BufRead, BufReader, Read, Seek, SeekFrom, Write};
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
pub const COMPACTED_JOURNAL_TARGET_BYTES: usize = 24 * 1024 * 1024;
pub const MAX_RETAINED_EXECUTIONS: usize = 2_000;
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
    Start {
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
    },
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
}

impl ExecutionEvent {
    fn version(&self) -> u32 {
        match self {
            Self::Start {
                jsh_execution_version,
                ..
            }
            | Self::Finish {
                jsh_execution_version,
                ..
            }
            | Self::Output {
                jsh_execution_version,
                ..
            } => *jsh_execution_version,
        }
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
        let path = select_journal_path(std::env::var_os("JSH_EXECUTION_JOURNAL_PATH"))?;
        Some(Self::with_path(path))
    }

    pub fn with_path(path: PathBuf) -> Self {
        let harden_existing_parent = default_journal_path().as_deref() == Some(path.as_path());
        let lock_path = path
            .parent()
            .unwrap_or_else(|| Path::new("."))
            .join("executions.lock");
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
        let (cwd, _) = bounded_text(cwd, MAX_CWD_BYTES);
        self.append_event(ExecutionEvent::Start {
            jsh_execution_version: EXECUTION_JOURNAL_VERSION,
            id: validate_execution_id(id)?.to_string(),
            session_id: match session_id {
                Some(id) => Some(validate_session_id(id)?.to_string()),
                None => None,
            },
            seq,
            command,
            command_truncated,
            cwd,
            started_at_ms,
        })
    }

    pub fn record_finish(
        &self,
        id: &str,
        exit_code: i32,
        duration_ms: u64,
        cwd_after: &str,
        ended_at_ms: u64,
    ) -> io::Result<()> {
        let (cwd_after, _) = bounded_text(cwd_after, MAX_CWD_BYTES);
        self.append_event(ExecutionEvent::Finish {
            jsh_execution_version: EXECUTION_JOURNAL_VERSION,
            id: validate_execution_id(id)?.to_string(),
            exit_code,
            duration_ms,
            cwd_after,
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
        let (mut text, limited) = bounded_text(text, MAX_OUTPUT_BYTES);
        let mut event = ExecutionEvent::Output {
            jsh_execution_version: EXECUTION_JOURNAL_VERSION,
            id: validate_execution_id(id)?.to_string(),
            text: text.clone(),
            truncated: truncated || limited,
            total_bytes: total_bytes.max(text.len() as u64),
            captured_at_ms,
        };
        // JSON escaping can expand control-heavy text beyond the line limit.
        // Shrink once more rather than writing an unreadable oversized event.
        if serde_json::to_vec(&event).map_err(io::Error::other)?.len() > MAX_EVENT_LINE_BYTES {
            (text, _) = bounded_text(&text, MAX_OUTPUT_BYTES / 2);
            let retained_bytes = text.len() as u64;
            event = ExecutionEvent::Output {
                jsh_execution_version: EXECUTION_JOURNAL_VERSION,
                id: id.to_string(),
                text,
                truncated: true,
                total_bytes: total_bytes.max(retained_bytes),
                captured_at_ms,
            };
        }
        self.append_event(event)
    }

    /// Fold the append-only event stream into one record per execution.
    /// Malformed, oversized, unknown-version, and orphan events are ignored.
    pub fn records(&self) -> io::Result<Vec<ExecutionRecord>> {
        if matches!(
            fs::symlink_metadata(&self.path),
            Err(error) if error.kind() == io::ErrorKind::NotFound
        ) {
            return Ok(Vec::new());
        }
        let _lock = self.lock(FlockArg::LockShared)?;
        match read_records(&self.path) {
            Ok(records) => Ok(records),
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
    Ok(())
}

/// The directory has to be the user's; it does not have to be private. See the
/// same function in `history.rs` for why the mode bits are not consulted — the
/// journal's own descriptor carries the same `O_NOFOLLOW`, one-link,
/// owned-by-this-user, 0600 guarantees.
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

fn set_private_open_file_permissions(file: &File) -> io::Result<()> {
    file.set_permissions(fs::Permissions::from_mode(0o600))
}

pub fn default_journal_path() -> Option<PathBuf> {
    let state_dir = dirs::state_dir()
        .or_else(|| dirs::home_dir().map(|home| home.join(".local").join("state")));
    state_dir.map(|state_dir| state_dir.join("jsh").join("executions.jsonl"))
}

fn select_journal_path(override_path: Option<std::ffi::OsString>) -> Option<PathBuf> {
    match override_path {
        Some(path) => {
            let path = PathBuf::from(path);
            (!path.as_os_str().is_empty() && path.is_absolute()).then_some(path)
        }
        None => default_journal_path(),
    }
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
    if !id.is_empty()
        && id.len() <= MAX_EXECUTION_ID_BYTES
        && id
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'_' | b'.'))
    {
        Ok(id)
    } else {
        Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "invalid execution ID",
        ))
    }
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

fn read_records(path: &Path) -> io::Result<Vec<ExecutionRecord>> {
    let file = open_regular_file(path, true, false, false)?;
    set_private_open_file_permissions(&file)?;
    if file.metadata()?.len() > MAX_JOURNAL_READ_BYTES {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "execution journal exceeds size limit",
        ));
    }
    let mut records = HashMap::<String, ExecutionRecord>::new();
    // Keep the working set bounded while preserving the newest start-event
    // chronology. A second start for one ID is authoritative and must move to
    // its new position rather than retaining the first event's eviction age.
    // The ordered index keeps both replacement and eviction logarithmic under
    // a hostile stream of tiny duplicate or unique records.
    let mut record_order = BTreeMap::<(u64, u64, String), String>::new();
    let mut reader = BufReader::new(file);
    let mut line = Vec::new();
    let mut bytes_read = 0_u64;
    while let Some(within_limit) = read_bounded_line(&mut reader, &mut line, &mut bytes_read)? {
        if !within_limit {
            continue;
        }
        let Ok(event) = serde_json::from_slice::<ExecutionEvent>(&line) else {
            continue;
        };
        if event.version() != EXECUTION_JOURNAL_VERSION {
            continue;
        }
        match event {
            ExecutionEvent::Start {
                id,
                session_id,
                seq,
                command,
                command_truncated,
                cwd,
                started_at_ms,
                ..
            } => {
                if validate_execution_id(&id).is_err()
                    || session_id
                        .as_deref()
                        .is_some_and(|id| validate_session_id(id).is_err())
                    || command.len() > MAX_COMMAND_BYTES
                    || cwd.len() > MAX_CWD_BYTES
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
                if let Some(previous) = records.remove(&id) {
                    record_order.remove(&(
                        previous.started_at_ms,
                        previous.seq,
                        previous.id.clone(),
                    ));
                }
                record_order.insert((started_at_ms, seq, id.clone()), id.clone());
                records.insert(id, record);
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
                if validate_execution_id(&id).is_err() || cwd_after.len() > MAX_CWD_BYTES {
                    continue;
                }
                if let Some(record) = records.get_mut(&id) {
                    record.exit_code = Some(exit_code);
                    record.duration_ms = Some(duration_ms);
                    record.cwd_after = Some(cwd_after);
                    record.ended_at_ms = Some(ended_at_ms);
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
                    record.output = Some(ExecutionOutput {
                        text,
                        truncated,
                        total_bytes,
                        captured_at_ms,
                    });
                }
            }
        }
    }
    let mut records = records.into_values().collect::<Vec<_>>();
    records.sort_by(|left, right| {
        (left.started_at_ms, left.seq, &left.id).cmp(&(right.started_at_ms, right.seq, &right.id))
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
    let records = read_records(path)?;
    let mut retained = Vec::<Vec<u8>>::new();
    let mut retained_bytes = 0usize;
    for record in records.iter().rev().take(MAX_RETAINED_EXECUTIONS) {
        let encoded = encode_compacted_record(record)?;
        if retained_bytes + encoded.len() > COMPACTED_JOURNAL_TARGET_BYTES {
            break;
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

fn encode_compacted_record(record: &ExecutionRecord) -> io::Result<Vec<u8>> {
    let mut encoded = Vec::new();
    write_compacted_event(
        &mut encoded,
        &ExecutionEvent::Start {
            jsh_execution_version: EXECUTION_JOURNAL_VERSION,
            id: record.id.clone(),
            session_id: record.session_id.clone(),
            seq: record.seq,
            command: record.command.clone(),
            command_truncated: record.command_truncated,
            cwd: record.cwd.clone(),
            started_at_ms: record.started_at_ms,
        },
    )?;
    if let (Some(exit_code), Some(duration_ms), Some(cwd_after), Some(ended_at_ms)) = (
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
    if let Some(output) = &record.output {
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
        writeln!(file, "{{\"jsh_execution_version\":1,\"event\":\"output\",\"id\":\"jsh-core-fixture\",\"text\":\"one\\ntwo\",\"truncated\":false,\"total_bytes\":7,\"captured_at_ms\":12}}").unwrap();
        drop(file);

        let record = journal.get("jsh-core-fixture").unwrap().unwrap();
        assert_eq!(record.command, "printf one\nprintf two");
        assert_eq!(record.output.unwrap().text, "one\ntwo");
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
            .record_output("jsh-bounded", &output, false, output.len() as u64, 2)
            .unwrap();
        let record = journal.get("jsh-bounded").unwrap().unwrap();
        assert_eq!(record.command.len(), MAX_COMMAND_BYTES);
        assert!(record.command_truncated);
        assert_eq!(record.output.as_ref().unwrap().text.len(), MAX_OUTPUT_BYTES);
        assert!(record.output.as_ref().unwrap().truncated);
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

    /// A shared parent directory says where the user keeps their state, not
    /// who owns the journal in it. The journal is still this user's and still
    /// 0600, established on its own descriptor.
    #[test]
    fn group_writable_custom_parent_is_accepted() {
        let dir = tempfile::tempdir().unwrap();
        let parent = dir.path().join("shared");
        fs::create_dir(&parent).unwrap();
        fs::set_permissions(&parent, fs::Permissions::from_mode(0o770)).unwrap();
        let journal = ExecutionJournal::with_path(parent.join("custom.jsonl"));

        journal
            .record_start("jsh-custom", None, 1, "true", "/tmp", 1)
            .expect("a shared parent is not a private file");

        assert_eq!(
            fs::metadata(journal.path()).unwrap().permissions().mode() & 0o7777,
            0o600
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
    fn journal_path_override_must_be_absolute() {
        assert!(select_journal_path(Some("relative/file.jsonl".into())).is_none());
        assert_eq!(
            select_journal_path(Some("/tmp/jsh-test/executions.jsonl".into())),
            Some(PathBuf::from("/tmp/jsh-test/executions.jsonl"))
        );
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
            &ExecutionEvent::Start {
                jsh_execution_version: EXECUTION_JOURNAL_VERSION,
                id: "jsh-after-large-line".into(),
                session_id: None,
                seq: 1,
                command: "echo recovered".into(),
                command_truncated: false,
                cwd: "/tmp".into(),
                started_at_ms: 1,
            },
        )
        .unwrap();
        drop(file);

        let records = journal.records().unwrap();
        assert_eq!(records.len(), 1);
        assert_eq!(records[0].id, "jsh-after-large-line");
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
                &ExecutionEvent::Start {
                    jsh_execution_version: EXECUTION_JOURNAL_VERSION,
                    id: format!("jsh-{seq}"),
                    session_id: Some("tab".into()),
                    seq,
                    command: format!("echo {seq}"),
                    command_truncated: false,
                    cwd: "/tmp".into(),
                    started_at_ms: seq,
                },
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
                &ExecutionEvent::Start {
                    jsh_execution_version: EXECUTION_JOURNAL_VERSION,
                    id: id.clone(),
                    session_id: Some("tab".into()),
                    seq,
                    command: "failing-command".into(),
                    command_truncated: false,
                    cwd: "/tmp".into(),
                    started_at_ms: seq,
                },
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
                &ExecutionEvent::Start {
                    jsh_execution_version: EXECUTION_JOURNAL_VERSION,
                    id: format!("jsh-bounded-{seq}"),
                    session_id: None,
                    seq,
                    command: "true".into(),
                    command_truncated: false,
                    cwd: "/tmp".into(),
                    started_at_ms: seq,
                },
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
