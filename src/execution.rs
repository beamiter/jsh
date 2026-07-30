//! Structured command execution journal shared with terminal emulators.
//!
//! The journal is deliberately separate from command history. It is an
//! append-only JSONL event stream so jsh and a terminal can safely contribute
//! metadata without redirecting a child's stdout/stderr away from its PTY.

use nix::fcntl::{Flock, FlockArg};
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::fs::{self, File, OpenOptions};
use std::io::{self, BufRead, BufReader, Write};
use std::os::unix::fs::{OpenOptionsExt, PermissionsExt};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{SystemTime, UNIX_EPOCH};

pub const EXECUTION_JOURNAL_VERSION: u32 = 1;
pub const MAX_COMMAND_BYTES: usize = 64 * 1024;
pub const MAX_CWD_BYTES: usize = 4 * 1024;
pub const MAX_OUTPUT_BYTES: usize = 256 * 1024;
pub const MAX_EVENT_LINE_BYTES: usize = 1024 * 1024;
pub const MAX_JOURNAL_FILE_BYTES: u64 = 32 * 1024 * 1024;
pub const COMPACTED_JOURNAL_TARGET_BYTES: usize = 24 * 1024 * 1024;
pub const MAX_RETAINED_EXECUTIONS: usize = 2_000;

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
        if !self.path.exists() {
            return Ok(Vec::new());
        }
        let _lock = self.lock(FlockArg::LockShared)?;
        read_records(&self.path)
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
        let mut file = OpenOptions::new()
            .create(true)
            .append(true)
            .mode(0o600)
            .open(&self.path)?;
        set_private_file_permissions(&self.path)?;
        file.write_all(&encoded)?;
        if file.metadata()?.len() > MAX_JOURNAL_FILE_BYTES {
            drop(file);
            compact_unlocked(&self.path)?;
        }
        Ok(())
    }

    fn lock(&self, arg: FlockArg) -> io::Result<Flock<File>> {
        if let Some(parent) = self.path.parent() {
            let parent_existed = parent.try_exists()?;
            fs::create_dir_all(parent)?;
            if !parent_existed || self.harden_existing_parent {
                fs::set_permissions(parent, fs::Permissions::from_mode(0o700))?;
            }
        }
        let file = OpenOptions::new()
            .read(true)
            .write(true)
            .create(true)
            .mode(0o600)
            .open(&self.lock_path)?;
        set_private_file_permissions(&self.lock_path)?;
        Flock::lock(file, arg).map_err(|(_, errno)| io::Error::from_raw_os_error(errno as i32))
    }
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
        && id.len() <= 192
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
    if !id.is_empty()
        && id.len() <= 128
        && id
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'_'))
    {
        Ok(id)
    } else {
        Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "invalid session ID",
        ))
    }
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
    let file = File::open(path)?;
    let mut records = Vec::<ExecutionRecord>::new();
    let mut indices = HashMap::<String, usize>::new();
    let mut reader = BufReader::new(file);
    let mut line = Vec::new();
    while let Some(within_limit) = read_bounded_line(&mut reader, &mut line)? {
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
                if let Some(index) = indices.get(&id).copied() {
                    records[index] = record;
                } else {
                    indices.insert(id, records.len());
                    records.push(record);
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
                if cwd_after.len() > MAX_CWD_BYTES {
                    continue;
                }
                if let Some(record) = indices.get(&id).and_then(|index| records.get_mut(*index)) {
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
                if text.len() > MAX_OUTPUT_BYTES {
                    continue;
                }
                if let Some(record) = indices.get(&id).and_then(|index| records.get_mut(*index)) {
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
    Ok(records)
}

/// Read and, when necessary, discard one JSONL record without allocating more
/// than the public per-event limit. `false` denotes an oversized record.
fn read_bounded_line(reader: &mut impl BufRead, line: &mut Vec<u8>) -> io::Result<Option<bool>> {
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
            if line.len() + consumed <= MAX_EVENT_LINE_BYTES + 1 {
                line.extend_from_slice(&buffer[..consumed]);
            } else {
                line.clear();
                oversized = true;
            }
        }
        reader.consume(consumed);
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
            .mode(0o600)
            .open(&tmp_path)?;
        for encoded in retained.iter().rev() {
            file.write_all(encoded)?;
        }
        file.sync_all()?;
        fs::rename(&tmp_path, path)?;
        set_private_file_permissions(path)
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

fn set_private_file_permissions(path: &Path) -> io::Result<()> {
    fs::set_permissions(path, fs::Permissions::from_mode(0o600))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn journal() -> (tempfile::TempDir, ExecutionJournal) {
        let dir = tempfile::tempdir().unwrap();
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
    fn compaction_keeps_the_most_recent_execution_records() {
        let (_dir, journal) = journal();
        let mut file = OpenOptions::new()
            .create(true)
            .write(true)
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
}
