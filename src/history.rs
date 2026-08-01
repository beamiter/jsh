/// History management: file I/O, in-memory ring, search.
/// Supports timestamped entries for rich Ctrl+R panel display.
use nix::fcntl::{Flock, FlockArg};
use serde::{Deserialize, Serialize};
use std::collections::{HashSet, VecDeque};
use std::fs::{self, File, OpenOptions};
use std::io::{self, IsTerminal, Read, Seek, SeekFrom, Write};
use std::os::unix::fs::{DirBuilderExt, OpenOptionsExt, PermissionsExt};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

type ScoredHistoryMatch = (String, Vec<usize>, i32, u64, Option<String>);

const HISTORY_RECORD_VERSION: u32 = 1;
/// A command is useful only while it remains reviewable and cheaply
/// serializable. This includes the JSONL newline so readers and writers agree
/// exactly at the boundary.
const MAX_HISTORY_RECORD_BYTES: usize = 1024 * 1024;
/// Bound startup and merge memory even if a local file is malformed or was
/// produced by an older, unbounded build.
const MAX_HISTORY_FILE_BYTES: usize = 32 * 1024 * 1024;
const MAX_HISTORY_ENTRIES: usize = 100_000;
const MAX_HISTORY_CWD_BYTES: usize = 64 * 1024;
const HISTORY_LOCK_TIMEOUT: Duration = Duration::from_secs(2);
const HISTORY_LOCK_RETRY: Duration = Duration::from_millis(10);
const SAFE_FILE_OPEN_FLAGS: i32 =
    nix::libc::O_NOFOLLOW | nix::libc::O_NONBLOCK | nix::libc::O_CLOEXEC;
static TEMP_FILE_COUNTER: AtomicU64 = AtomicU64::new(0);
/// One hint per process, however many `History` instances get built.
static LEGACY_HISTORY_HINT_SHOWN: AtomicBool = AtomicBool::new(false);
static HISTORY_IO_WARNING_SHOWN: AtomicBool = AtomicBool::new(false);

#[derive(Clone, Debug, Eq, Hash, PartialEq)]
pub struct HistoryEntry {
    pub command: String,
    pub timestamp: u64,
    pub cwd: Option<String>,
}

/// On-disk JSONL envelope. Keeping a format marker separate from
/// `HistoryEntry` lets us distinguish new records from legacy commands that
/// happen to contain JSON.
#[derive(Deserialize, Serialize)]
struct HistoryRecord {
    /// The field was named `rsh_history_version` before the 0.2.0 rename. The
    /// alias is what makes a pre-rename history file readable at all: without
    /// it the record fails to deserialize, falls through to the legacy
    /// tab-separated branch, and each JSON line is stored as if the whole line
    /// were the command the user typed.
    #[serde(alias = "rsh_history_version")]
    jsh_history_version: u32,
    command: String,
    timestamp: u64,
    cwd: Option<String>,
}

impl From<&HistoryEntry> for HistoryRecord {
    fn from(entry: &HistoryEntry) -> Self {
        Self {
            jsh_history_version: HISTORY_RECORD_VERSION,
            command: entry.command.clone(),
            timestamp: entry.timestamp,
            cwd: entry.cwd.clone(),
        }
    }
}

pub struct History {
    entries: Vec<HistoryEntry>,
    max_size: usize,
    file_path: PathBuf,
    position: usize,
}

impl History {
    pub fn new(max_size: usize) -> Self {
        // The pre-rename ~/.rsh_history has to be in place before the first
        // read, or an interactive shell starts with an empty history panel and
        // the user assumes the data is gone.
        crate::config::migrate_legacy_rsh_data();

        let home = dirs::home_dir().unwrap_or_else(|| {
            // A shared /tmp parent is intentionally rejected by the directory
            // ownership check. Use a per-user fallback namespace instead.
            // SAFETY: geteuid has no preconditions and only reads process state.
            let uid = unsafe { nix::libc::geteuid() };
            std::env::temp_dir().join(format!("jsh-{uid}"))
        });
        let file_path = home.join(".jsh_history");

        let history = Self::new_with_path(max_size, file_path);
        history.warn_about_unimported_legacy_history(&home);
        history
    }

    /// Load decoded entries from the default history file. This is the
    /// compatibility boundary for non-editor consumers such as the `history`
    /// builtin and command completion; callers never need to understand the
    /// JSONL or legacy on-disk formats.
    pub fn load_default_entries(max_size: usize) -> Vec<HistoryEntry> {
        Self::new(max_size).entries
    }

    /// Tell the user, once, when a pre-rename history file was left behind.
    ///
    /// The automatic migration only ever copies into a path that does not
    /// exist yet, because it must never clobber newer data. That leaves one
    /// case uncovered: a user who already started the renamed binary has a
    /// small fresh ~/.jsh_history, so their full ~/.rsh_history will never be
    /// read by anything. Detecting that is read-only — importing stays the
    /// user's decision, since merging two histories is not something to do
    /// behind their back.
    fn warn_about_unimported_legacy_history(&self, home: &Path) {
        if LEGACY_HISTORY_HINT_SHOWN.swap(true, Ordering::SeqCst) {
            return;
        }
        // A hint is for a human at a terminal. Scripts compare stderr.
        if !io::stderr().is_terminal() {
            return;
        }
        let legacy_path = crate::config::legacy_history_path(home);
        let Ok(legacy_entries) = read_entries(&legacy_path, MAX_HISTORY_ENTRIES) else {
            return;
        };
        // The newest legacy record is the one that survives history trimming,
        // so its presence is a reliable "already imported" marker.
        let Some(newest) = legacy_entries.iter().max_by_key(|entry| entry.timestamp) else {
            return;
        };
        if self.entries.contains(newest) {
            return;
        }
        // `Path`'s Debug representation quotes and escapes arbitrary bytes.
        // Do not print a copy/paste shell command here: even correct-looking
        // paths may contain whitespace, control bytes, or shell metacharacters.
        eprintln!(
            "jsh: {legacy_path:?} holds {} command(s) from before the rsh->jsh rename, and {:?} already existed, so they were not imported automatically.",
            legacy_entries.len(),
            self.file_path
        );
        eprintln!(
            "jsh: review {legacy_path:?} and append it to {:?} manually if desired.",
            self.file_path
        );
    }

    fn new_with_path(max_size: usize, file_path: PathBuf) -> Self {
        let mut h = History {
            entries: Vec::new(),
            max_size: max_size.min(MAX_HISTORY_ENTRIES),
            file_path,
            position: 0,
        };
        h.load();
        h
    }

    fn load(&mut self) {
        // Updated jsh processes coordinate through a stable sidecar lock.
        // Atomic rewrites already protect readers from torn files; the lock
        // additionally keeps us from racing an append at startup.
        if matches!(
            fs::symlink_metadata(&self.file_path),
            Err(error) if error.kind() == io::ErrorKind::NotFound
        ) {
            self.entries.clear();
            self.position = 0;
            return;
        }
        self.entries = match lock_history_file(&self.file_path) {
            Ok(_lock) => match read_entries_or_empty(&self.file_path, self.max_size) {
                Ok(entries) => entries,
                Err(error) => {
                    warn_history_io("load", &self.file_path, &error);
                    Vec::new()
                }
            },
            Err(error) => {
                warn_history_io("lock", &self.file_path, &error);
                Vec::new()
            }
        };
        self.position = self.entries.len();
    }

    fn parse_line(line: &str) -> Option<HistoryEntry> {
        if line.is_empty() {
            return None;
        }

        if let Ok(record) = serde_json::from_str::<HistoryRecord>(line) {
            return (record.jsh_history_version == HISTORY_RECORD_VERSION)
                .then_some(HistoryEntry {
                    command: record.command,
                    timestamp: record.timestamp,
                    cwd: record.cwd,
                })
                .and_then(normalize_history_entry);
        }

        // Legacy format: "timestamp\tcwd\tcommand" or a plain command.
        let parts: Vec<&str> = line.splitn(3, '\t').collect();
        if parts.len() == 3 {
            if let Ok(ts) = parts[0].parse::<u64>() {
                let cwd = if parts[1].is_empty() {
                    None
                } else {
                    Some(parts[1].to_string())
                };
                return normalize_history_entry(HistoryEntry {
                    command: parts[2].to_string(),
                    timestamp: ts,
                    cwd,
                });
            }
        }
        normalize_history_entry(HistoryEntry {
            command: line.to_string(),
            timestamp: 0,
            cwd: None,
        })
    }

    fn format_entry(entry: &HistoryEntry) -> io::Result<String> {
        serde_json::to_string(&HistoryRecord::from(entry)).map_err(io::Error::other)
    }

    pub fn save(&self) {
        if let Err(error) = self.save_inner() {
            warn_history_io("save", &self.file_path, &error);
        }
    }

    fn save_inner(&self) -> io::Result<()> {
        let _lock = lock_history_file(&self.file_path)?;

        // Merge with what is currently on disk while holding the lock. This
        // preserves commands appended by shells launched after this instance.
        let mut merged = read_entries_or_empty(&self.file_path, MAX_HISTORY_ENTRIES)?;
        let mut seen: HashSet<HistoryEntry> = merged.iter().cloned().collect();
        for entry in &self.entries {
            if seen.insert(entry.clone()) {
                merged.push(entry.clone());
            }
        }
        // A long-running shell may reintroduce old in-memory entries after a
        // different shell pruned the file. Timestamp ordering keeps those old
        // records at the front so the shared limit remains meaningful.
        merged.sort_by_key(|entry| entry.timestamp);
        trim_to_storage_limits(&mut merged, self.max_size, MAX_HISTORY_FILE_BYTES)?;
        write_entries_atomically(&self.file_path, &merged)
    }

    pub fn add(&mut self, entry: &str) {
        self.add_with_cwd(entry, None);
    }

    pub fn add_with_cwd(&mut self, entry: &str, cwd: Option<&str>) {
        if self.max_size == 0 {
            return;
        }
        let command = entry.trim().to_string();
        if command.is_empty() {
            return;
        }
        if self.entries.last().map(|e| e.command.as_str()) == Some(&command) {
            return;
        }

        let timestamp = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map(|d| d.as_secs())
            .unwrap_or(0);

        let Some(he) = normalize_history_entry(HistoryEntry {
            command: command.clone(),
            timestamp,
            cwd: cwd.map(|s| s.to_string()),
        }) else {
            warn_history_io(
                "append",
                &self.file_path,
                &io::Error::new(
                    io::ErrorKind::InvalidInput,
                    "history command contains terminal-ambiguous text",
                ),
            );
            return;
        };
        // Never retain a command in memory that this same build refuses to
        // persist: history expansion must not expose a truncated surrogate.
        if let Err(error) = encode_entry(&he) {
            warn_history_io("append", &self.file_path, &error);
            return;
        }

        self.entries.push(he.clone());
        // The in-memory history is a queue too. Keep it under both the entry
        // ceiling and the same aggregate byte ceiling as the persisted JSONL,
        // so a stream of individually valid near-1-MiB commands cannot grow a
        // long-lived interactive shell without bound.
        if let Err(error) =
            trim_to_storage_limits(&mut self.entries, self.max_size, MAX_HISTORY_FILE_BYTES)
        {
            self.entries.pop();
            warn_history_io("append", &self.file_path, &error);
            return;
        }
        self.position = self.entries.len();

        if let Err(error) = append_entry(&self.file_path, &he, self.max_size) {
            warn_history_io("append", &self.file_path, &error);
        }
    }

    pub fn last(&self) -> Option<&str> {
        self.entries.last().map(|e| e.command.as_str())
    }

    pub fn get(&self, index: usize) -> Option<&str> {
        self.entries.get(index).map(|e| e.command.as_str())
    }

    pub fn entries(&self) -> Vec<&str> {
        self.entries.iter().map(|e| e.command.as_str()).collect()
    }

    pub fn len(&self) -> usize {
        self.entries.len()
    }

    pub fn is_empty(&self) -> bool {
        self.entries.is_empty()
    }

    pub fn prev(&mut self) -> Option<&str> {
        if self.position > 0 {
            self.position -= 1;
            Some(&self.entries[self.position].command)
        } else {
            None
        }
    }

    #[allow(clippy::should_implement_trait)]
    pub fn next(&mut self) -> Option<&str> {
        if !self.entries.is_empty() && self.position + 1 < self.entries.len() {
            self.position += 1;
            Some(&self.entries[self.position].command)
        } else {
            self.position = self.entries.len();
            None
        }
    }

    pub fn reset_position(&mut self) {
        self.position = self.entries.len();
    }

    pub fn search_prefix(&self, prefix: &str) -> Option<&str> {
        if prefix.is_empty() {
            return None;
        }
        for entry in self.entries.iter().rev() {
            if entry.command.starts_with(prefix) && entry.command.len() > prefix.len() {
                return Some(&entry.command);
            }
        }
        None
    }

    /// Prefer a matching command previously used in the current directory,
    /// then fall back to the global history. This keeps project-specific build
    /// and deploy commands from leaking into unrelated repositories.
    pub fn search_prefix_in_cwd(&self, prefix: &str, cwd: &str) -> Option<&str> {
        if prefix.is_empty() {
            return None;
        }
        self.entries
            .iter()
            .rev()
            .find(|entry| {
                entry.cwd.as_deref() == Some(cwd)
                    && entry.command.starts_with(prefix)
                    && entry.command.len() > prefix.len()
            })
            .map(|entry| entry.command.as_str())
            .or_else(|| self.search_prefix(prefix))
    }

    pub fn search_substring(&self, query: &str) -> Vec<&str> {
        if query.is_empty() {
            return Vec::new();
        }
        self.entries
            .iter()
            .rev()
            .filter(|e| e.command.contains(query))
            .map(|e| e.command.as_str())
            .collect()
    }

    /// Fuzzy search with metadata: returns (command, matched_indices, timestamp, cwd).
    pub fn search_fuzzy(&self, query: &str) -> Vec<(String, Vec<usize>)> {
        self.search_fuzzy_rich(query)
            .into_iter()
            .map(|(cmd, idx, _, _)| (cmd, idx))
            .collect()
    }

    pub fn search_fuzzy_rich(&self, query: &str) -> Vec<(String, Vec<usize>, u64, Option<String>)> {
        if query.is_empty() {
            return Vec::new();
        }
        let query_lower: Vec<char> = query.to_lowercase().chars().collect();
        let mut results: Vec<ScoredHistoryMatch> = Vec::new();

        for entry in self.entries.iter().rev() {
            let entry_lower: Vec<char> = entry.command.to_lowercase().chars().collect();
            if let Some((indices, score)) = fuzzy_match_score(&query_lower, &entry_lower) {
                if !results.iter().any(|(e, _, _, _, _)| e == &entry.command) {
                    results.push((
                        entry.command.clone(),
                        indices,
                        score,
                        entry.timestamp,
                        entry.cwd.clone(),
                    ));
                }
            }
            if results.len() >= 20 {
                break;
            }
        }

        results.sort_by_key(|entry| std::cmp::Reverse(entry.2));
        results
            .into_iter()
            .map(|(cmd, idx, _, ts, cwd)| (cmd, idx, ts, cwd))
            .collect()
    }

    pub fn format_relative_time(timestamp: u64) -> String {
        if timestamp == 0 {
            return String::new();
        }
        let now = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map(|d| d.as_secs())
            .unwrap_or(0);
        let diff = now.saturating_sub(timestamp);
        if diff < 60 {
            return format!("{}s", diff);
        }
        if diff < 3600 {
            return format!("{}m", diff / 60);
        }
        if diff < 86400 {
            return format!("{}h", diff / 3600);
        }
        if diff < 604800 {
            return format!("{}d", diff / 86400);
        }
        format!("{}w", diff / 604800)
    }
}

fn warn_history_io(operation: &str, path: &Path, error: &io::Error) {
    if !HISTORY_IO_WARNING_SHOWN.swap(true, Ordering::SeqCst) {
        eprintln!(
            "jsh: history {operation} failed for {path:?}: {error}; command execution will continue"
        );
    }
}

fn trim_to_limit(entries: &mut Vec<HistoryEntry>, max_size: usize) {
    let max_size = max_size.min(MAX_HISTORY_ENTRIES);
    if entries.len() > max_size {
        let remove = entries.len() - max_size;
        entries.drain(..remove);
    }
}

/// History is rendered by the interactive editor and can later become an
/// executable suggestion. Preserve structural newlines, but reject terminal
/// controls and invisible/bidirectional formatting that could disguise a
/// recalled command. Unsafe cwd metadata is nonessential, so drop it instead
/// of losing an otherwise valid command.
fn normalize_history_entry(mut entry: HistoryEntry) -> Option<HistoryEntry> {
    if entry.command.trim().is_empty()
        || entry
            .command
            .chars()
            .any(|ch| ch != '\n' && crate::terminal_text::is_terminal_ambiguous(ch))
    {
        return None;
    }
    if entry.cwd.as_ref().is_some_and(|cwd| {
        cwd.len() > MAX_HISTORY_CWD_BYTES
            || cwd
                .chars()
                .any(|ch| ch != '\t' && crate::terminal_text::is_terminal_ambiguous(ch))
    }) {
        entry.cwd = None;
    }
    Some(entry)
}

fn trim_to_storage_limits(
    entries: &mut Vec<HistoryEntry>,
    max_entries: usize,
    max_bytes: usize,
) -> io::Result<()> {
    trim_to_limit(entries, max_entries);
    let mut retained_bytes = 0usize;
    let mut keep_from = entries.len();
    for (index, entry) in entries.iter().enumerate().rev() {
        let encoded_bytes = encode_entry(entry)?.len();
        if retained_bytes.saturating_add(encoded_bytes) > max_bytes {
            break;
        }
        retained_bytes += encoded_bytes;
        keep_from = index;
    }
    entries.drain(..keep_from);
    Ok(())
}

fn read_entries(path: &Path, max_entries: usize) -> io::Result<Vec<HistoryEntry>> {
    let mut file = open_regular_file(path, true, false, false)?;
    set_private_open_file_permissions(&file)?;
    if file.metadata()?.len() > MAX_HISTORY_FILE_BYTES as u64 {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "history file exceeds size limit",
        ));
    }

    let mut bytes = Vec::new();
    (&mut file)
        .take((MAX_HISTORY_FILE_BYTES + 1) as u64)
        .read_to_end(&mut bytes)?;
    if bytes.len() > MAX_HISTORY_FILE_BYTES {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "history file grew beyond size limit while being read",
        ));
    }
    let text = std::str::from_utf8(&bytes).map_err(|error| {
        io::Error::new(
            io::ErrorKind::InvalidData,
            format!("history is not valid UTF-8: {error}"),
        )
    })?;
    let max_entries = max_entries.min(MAX_HISTORY_ENTRIES);
    let mut entries = VecDeque::with_capacity(max_entries.min(10_000));
    for line in text.lines() {
        if line.len().saturating_add(1) > MAX_HISTORY_RECORD_BYTES {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "history record exceeds size limit",
            ));
        }
        if let Some(entry) = History::parse_line(line) {
            if entries.len() == max_entries {
                entries.pop_front();
            }
            if max_entries != 0 {
                entries.push_back(entry);
            }
        }
    }
    Ok(entries.into())
}

fn read_entries_or_empty(path: &Path, max_entries: usize) -> io::Result<Vec<HistoryEntry>> {
    match read_entries(path, max_entries) {
        Ok(entries) => Ok(entries),
        Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(Vec::new()),
        Err(error) => Err(error),
    }
}

fn encode_entry(entry: &HistoryEntry) -> io::Result<Vec<u8>> {
    if normalize_history_entry(entry.clone()).as_ref() != Some(entry) {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "history entry contains terminal-ambiguous or oversized metadata",
        ));
    }
    let mut record = History::format_entry(entry)?.into_bytes();
    record.push(b'\n');
    if record.len() > MAX_HISTORY_RECORD_BYTES {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "history record exceeds size limit",
        ));
    }
    Ok(record)
}

fn append_entry(path: &Path, entry: &HistoryEntry, max_entries: usize) -> io::Result<()> {
    let record = encode_entry(entry)?;
    let _lock = lock_history_file(path)?;
    let mut file = open_regular_file(path, true, true, true)?;
    set_private_open_file_permissions(&file)?;

    // Build the complete record first, then issue one append write. O_APPEND
    // plus the sidecar lock prevents updated jsh processes from interleaving
    // JSON records.
    let current_bytes = usize::try_from(file.metadata()?.len()).unwrap_or(usize::MAX);
    let needs_separator = if current_bytes == 0 {
        false
    } else {
        file.seek(SeekFrom::End(-1))?;
        let mut last = [0_u8; 1];
        file.read_exact(&mut last)?;
        last[0] != b'\n'
    };
    if current_bytes
        .saturating_add(usize::from(needs_separator))
        .saturating_add(record.len())
        <= MAX_HISTORY_FILE_BYTES
    {
        let mut append_record = Vec::with_capacity(record.len() + usize::from(needs_separator));
        if needs_separator {
            append_record.push(b'\n');
        }
        append_record.extend_from_slice(&record);
        return file.write_all(&append_record);
    }

    // Compact while the same process lock is still held. The new command is
    // retained preferentially and records keep their chronological order.
    drop(file);
    let mut entries = read_entries_or_empty(path, MAX_HISTORY_ENTRIES)?;
    if !entries.contains(entry) {
        entries.push(entry.clone());
    }
    entries.sort_by_key(|candidate| candidate.timestamp);
    trim_to_storage_limits(&mut entries, max_entries, MAX_HISTORY_FILE_BYTES)?;
    write_entries_atomically(path, &entries)
}

fn write_entries_atomically(path: &Path, entries: &[HistoryEntry]) -> io::Result<()> {
    let encoded = entries
        .iter()
        .map(encode_entry)
        .collect::<io::Result<Vec<_>>>()?;
    let encoded_bytes = encoded
        .iter()
        .try_fold(0usize, |total, record| total.checked_add(record.len()))
        .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidInput, "history size overflow"))?;
    if encoded_bytes > MAX_HISTORY_FILE_BYTES {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "history file exceeds size limit",
        ));
    }
    let counter = TEMP_FILE_COUNTER.fetch_add(1, Ordering::Relaxed);
    let timestamp = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|duration| duration.as_nanos())
        .unwrap_or(0);
    let tmp_path = path.with_extension(format!(
        "tmp.{}.{}.{}",
        std::process::id(),
        timestamp,
        counter
    ));
    let result = (|| {
        let mut file = OpenOptions::new()
            .write(true)
            .create_new(true)
            .custom_flags(nix::libc::O_NOFOLLOW | nix::libc::O_CLOEXEC)
            .mode(0o600)
            .open(&tmp_path)?;
        set_private_open_file_permissions(&file)?;
        for record in &encoded {
            file.write_all(record)?;
        }
        file.sync_all()?;
        fs::rename(&tmp_path, path)?;
        if let Some(parent) = path.parent() {
            if let Ok(dir) = File::open(parent) {
                let _ = dir.sync_all();
            }
        }
        Ok(())
    })();
    if result.is_err() {
        let _ = fs::remove_file(&tmp_path);
    }
    result
}

struct HistoryFileLock {
    _directory: Flock<File>,
    file: Flock<File>,
}

fn lock_history_file(path: &Path) -> io::Result<HistoryFileLock> {
    lock_history_file_with_timeout(path, HISTORY_LOCK_TIMEOUT)
}

fn lock_history_file_with_timeout(path: &Path, timeout: Duration) -> io::Result<HistoryFileLock> {
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
    if !existed {
        directory.set_permissions(fs::Permissions::from_mode(0o700))?;
    }
    ensure_non_writable_directory(&directory, parent)?;
    // Updated peers lock the directory before opening the sidecar, so one of
    // them cannot rename it and obtain a different lock inode mid-operation.
    let directory = flock_exclusive_with_timeout(directory, timeout)?;

    let mut lock_name = path.as_os_str().to_os_string();
    lock_name.push(".lock");
    let lock_path = PathBuf::from(lock_name);
    let file = OpenOptions::new()
        .read(true)
        .write(true)
        .create(true)
        .truncate(false)
        .custom_flags(SAFE_FILE_OPEN_FLAGS)
        .mode(0o600)
        .open(&lock_path)?;
    ensure_regular_file(&file, &lock_path)?;
    set_private_open_file_permissions(&file)?;
    let file = flock_exclusive_with_timeout(file, timeout)?;
    Ok(HistoryFileLock {
        _directory: directory,
        file,
    })
}

fn flock_exclusive_with_timeout(mut file: File, timeout: Duration) -> io::Result<Flock<File>> {
    let started = Instant::now();
    loop {
        match Flock::lock(file, FlockArg::LockExclusiveNonblock) {
            Ok(lock) => return Ok(lock),
            Err((returned, errno)) => {
                file = returned;
                if errno != nix::errno::Errno::EAGAIN {
                    return Err(io::Error::from_raw_os_error(errno as i32));
                }
                if started.elapsed() >= timeout {
                    return Err(io::Error::new(
                        io::ErrorKind::WouldBlock,
                        "timed out waiting for history lock",
                    ));
                }
                let remaining = timeout
                    .checked_sub(started.elapsed())
                    .unwrap_or(Duration::ZERO);
                std::thread::sleep(HISTORY_LOCK_RETRY.min(remaining));
            }
        }
    }
}

fn open_regular_file(path: &Path, read: bool, append: bool, create: bool) -> io::Result<File> {
    let file = OpenOptions::new()
        .read(read)
        .append(append)
        .create(create)
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

fn ensure_non_writable_directory(file: &File, path: &Path) -> io::Result<()> {
    use std::os::unix::fs::MetadataExt;

    if file.metadata()?.mode() & 0o022 != 0 {
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

fn fuzzy_match_score(query: &[char], candidate: &[char]) -> Option<(Vec<usize>, i32)> {
    let mut qi = 0;
    let mut indices = Vec::new();
    let mut score: i32 = 0;
    let mut prev_match_idx: Option<usize> = None;

    for (ci, &cc) in candidate.iter().enumerate() {
        if qi < query.len() && cc == query[qi] {
            indices.push(ci);
            // Consecutive match bonus
            if let Some(prev) = prev_match_idx {
                if ci == prev + 1 {
                    score += 5;
                }
            }
            // Word boundary bonus
            if ci == 0 || !candidate[ci - 1].is_alphanumeric() {
                score += 3;
            }
            // Prefix bonus
            if ci == qi {
                score += 2;
            }
            prev_match_idx = Some(ci);
            qi += 1;
        }
    }

    if qi == query.len() {
        // Shorter candidates score higher (more relevant)
        score += (100i32).saturating_sub(candidate.len() as i32);
        Some((indices, score))
    } else {
        None
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use nix::sys::stat::Mode;
    use nix::unistd::mkfifo;
    use std::os::fd::AsRawFd;
    use std::os::unix::fs::{symlink, PermissionsExt};

    fn load_test_history(path: &Path, max_size: usize) -> History {
        if let Some(parent) = path.parent().filter(|parent| parent.is_dir()) {
            fs::set_permissions(parent, fs::Permissions::from_mode(0o700))
                .expect("private history fixture parent");
        }
        History::new_with_path(max_size, path.to_path_buf())
    }

    #[test]
    fn prefix_search_prefers_current_directory_then_falls_back() {
        let history = History {
            entries: vec![
                HistoryEntry {
                    command: "cargo test --workspace".into(),
                    timestamp: 1,
                    cwd: Some("/project/a".into()),
                },
                HistoryEntry {
                    command: "cargo test --release".into(),
                    timestamp: 2,
                    cwd: Some("/project/b".into()),
                },
            ],
            max_size: 10,
            file_path: PathBuf::new(),
            position: 2,
        };

        assert_eq!(
            history.search_prefix_in_cwd("cargo t", "/project/a"),
            Some("cargo test --workspace")
        );
        assert_eq!(
            history.search_prefix_in_cwd("cargo t", "/project/unknown"),
            Some("cargo test --release")
        );
    }

    #[test]
    fn jsonl_roundtrip_preserves_multiline_commands_and_metadata() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("history");
        let mut history = load_test_history(&path, 10);
        history.add_with_cwd("if true\nthen echo \"hello\"\nfi", Some("/tmp/a\tb"));
        history.save();

        let restored = load_test_history(&path, 10);
        assert_eq!(restored.len(), 1);
        assert_eq!(restored.last(), Some("if true\nthen echo \"hello\"\nfi"));
        assert_eq!(restored.entries[0].cwd.as_deref(), Some("/tmp/a\tb"));

        let on_disk = fs::read_to_string(&path).expect("history JSONL");
        assert_eq!(on_disk.lines().count(), 1);
        assert!(on_disk.contains("\\n"));
        assert!(on_disk.contains("jsh_history_version"));
    }

    #[test]
    fn loader_accepts_legacy_and_mixed_history_records() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("history");
        let json_entry = HistoryEntry {
            command: "printf 'new\\nrecord'".into(),
            timestamp: 12,
            cwd: Some("/new".into()),
        };
        let content = format!(
            "plain legacy\n10\t/old\tlegacy with metadata\n{}\n",
            History::format_entry(&json_entry).expect("serialize")
        );
        fs::write(&path, content).expect("fixture");

        let history = load_test_history(&path, 10);
        assert_eq!(history.len(), 3);
        assert_eq!(history.entries[0].command, "plain legacy");
        assert_eq!(history.entries[1].timestamp, 10);
        assert_eq!(history.entries[1].cwd.as_deref(), Some("/old"));
        assert_eq!(history.entries[2], json_entry);
    }

    /// A pre-rename history file uses `rsh_history_version`. Without the serde
    /// alias the record fails to deserialize, the loader falls through to the
    /// legacy tab-separated branch, and every entry becomes a "command" whose
    /// text is the raw JSON line — with timestamp 0 and no cwd. That is what a
    /// hand-renamed ~/.rsh_history did before this fix.
    #[test]
    fn pre_rename_history_records_keep_command_timestamp_and_cwd() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("history");
        fs::write(
            &path,
            concat!(
                r#"{"rsh_history_version":1,"command":"echo one","timestamp":10,"cwd":"/p"}"#,
                "\n",
                r#"{"rsh_history_version":1,"command":"if true\nthen echo hi\nfi","timestamp":11,"cwd":"/q"}"#,
                "\n",
            ),
        )
        .expect("pre-rename fixture");

        let history = load_test_history(&path, 10);

        assert_eq!(history.len(), 2);
        assert_eq!(
            history.entries[0],
            HistoryEntry {
                command: "echo one".into(),
                timestamp: 10,
                cwd: Some("/p".into()),
            }
        );
        assert_eq!(
            history.entries[1],
            HistoryEntry {
                command: "if true\nthen echo hi\nfi".into(),
                timestamp: 11,
                cwd: Some("/q".into()),
            }
        );
    }

    /// The realistic post-import state: pre-rename records concatenated onto a
    /// file that already holds new-format ones, plus a corrupt line. Nothing
    /// may be dropped and saving must not lose either side.
    #[test]
    fn mixed_pre_and_post_rename_records_survive_a_save() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("history");
        fs::write(
            &path,
            concat!(
                r#"{"jsh_history_version":1,"command":"ls","timestamp":30,"cwd":"/new"}"#,
                "\n",
                "{not json at all\n",
                r#"{"rsh_history_version":1,"command":"echo old","timestamp":10,"cwd":"/old"}"#,
                "\n",
            ),
        )
        .expect("mixed fixture");

        let history = load_test_history(&path, 10);
        history.save();

        let restored = load_test_history(&path, 10);
        let commands = restored.entries();
        assert!(commands.contains(&"ls"), "{commands:?}");
        assert!(commands.contains(&"echo old"), "{commands:?}");
        // The unparsable line is preserved verbatim rather than discarded.
        assert!(commands.contains(&"{not json at all"), "{commands:?}");
        let cwds: Vec<Option<&str>> = restored
            .entries
            .iter()
            .map(|entry| entry.cwd.as_deref())
            .collect();
        assert!(cwds.contains(&Some("/old")), "{cwds:?}");
    }

    /// A corrupt pre-rename file must not take the shell down, and must not
    /// affect the current history file.
    #[test]
    fn a_corrupt_pre_rename_file_does_not_disturb_the_current_history() {
        let temp = tempfile::tempdir().expect("tempdir");
        let home = temp.path().join("home");
        let state = temp.path().join("state");
        fs::create_dir_all(&home).expect("home");
        fs::write(home.join(".rsh_history"), b"\x00\x01 garbage \xff\n").expect("corrupt fixture");
        fs::write(
            home.join(".jsh_history"),
            r#"{"jsh_history_version":1,"command":"keep","timestamp":5,"cwd":null}"#.to_owned()
                + "\n",
        )
        .expect("current history");

        let report = crate::config::migrate_legacy_rsh_data_in(&home, &state);
        assert!(report.migrated.is_empty(), "{report:?}");

        let history = load_test_history(&home.join(".jsh_history"), 10);
        assert_eq!(history.entries(), vec!["keep"]);
    }

    #[test]
    fn save_merges_entries_appended_by_another_shell() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("history");
        let mut first = load_test_history(&path, 10);
        let mut second = load_test_history(&path, 10);

        first.add("echo from-first");
        second.add("echo from-second");
        first.save();

        let restored = load_test_history(&path, 10);
        let commands = restored.entries();
        assert!(commands.contains(&"echo from-first"));
        assert!(commands.contains(&"echo from-second"));
    }

    #[test]
    fn append_after_an_unterminated_tail_keeps_the_new_command_separate() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("history");
        fs::write(&path, "legacy tail without newline").expect("history fixture");
        fs::set_permissions(&path, fs::Permissions::from_mode(0o600)).expect("history mode");
        let mut history = load_test_history(&path, 10);

        history.add("echo after-tail");

        let restored = load_test_history(&path, 10);
        assert_eq!(
            restored.entries(),
            vec!["legacy tail without newline", "echo after-tail"]
        );
    }

    #[test]
    fn history_and_lock_files_are_private() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("history");
        let mut history = load_test_history(&path, 10);
        history.add("echo private");
        fs::set_permissions(&path, fs::Permissions::from_mode(0o644)).expect("loosen fixture");
        history.save();

        let mode = fs::metadata(&path)
            .expect("history metadata")
            .permissions()
            .mode()
            & 0o777;
        assert_eq!(mode, 0o600);
        let lock_mode = fs::metadata(dir.path().join("history.lock"))
            .expect("lock metadata")
            .permissions()
            .mode()
            & 0o777;
        assert_eq!(lock_mode, 0o600);
    }

    #[test]
    fn writing_history_creates_a_missing_parent_directory() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("missing").join("nested").join("history");
        let mut history = load_test_history(&path, 10);
        history.add("echo creates-parent");

        let restored = load_test_history(&path, 10);
        assert_eq!(restored.last(), Some("echo creates-parent"));
        assert!(path.exists());
    }

    #[test]
    fn history_rejects_a_group_writable_parent_namespace() {
        let dir = tempfile::tempdir().expect("tempdir");
        let parent = dir.path().join("shared");
        fs::create_dir(&parent).expect("shared parent");
        fs::set_permissions(&parent, fs::Permissions::from_mode(0o770))
            .expect("shared parent mode");
        let path = parent.join("history");
        let entry = HistoryEntry {
            command: "echo private".into(),
            timestamp: 1,
            cwd: None,
        };

        let error = append_entry(&path, &entry, 10).expect_err("unsafe namespace accepted");

        assert_eq!(error.kind(), io::ErrorKind::PermissionDenied);
        assert!(!path.exists());
    }

    #[test]
    fn history_file_symlink_never_exposes_commands_or_changes_the_target() {
        let dir = tempfile::tempdir().expect("tempdir");
        let victim = dir.path().join("victim");
        let path = dir.path().join("history");
        fs::write(&victim, "keep me\n").expect("victim");
        fs::set_permissions(&victim, fs::Permissions::from_mode(0o640)).expect("victim mode");
        symlink(&victim, &path).expect("history symlink");

        let mut history = load_test_history(&path, 10);
        assert!(history.is_empty());
        history.add("secret command");
        history.save();

        assert!(fs::symlink_metadata(&path)
            .expect("history link")
            .file_type()
            .is_symlink());
        assert_eq!(
            fs::read_to_string(&victim).expect("victim contents"),
            "keep me\n"
        );
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
    fn history_lock_symlink_never_changes_the_target() {
        let dir = tempfile::tempdir().expect("tempdir");
        let victim = dir.path().join("victim");
        let path = dir.path().join("history");
        let lock_path = dir.path().join("history.lock");
        fs::write(&victim, "keep me\n").expect("victim");
        fs::set_permissions(&victim, fs::Permissions::from_mode(0o640)).expect("victim mode");
        symlink(&victim, &lock_path).expect("lock symlink");

        let mut history = load_test_history(&path, 10);
        history.add("secret command");

        assert!(!path.exists());
        assert_eq!(
            fs::read_to_string(&victim).expect("victim contents"),
            "keep me\n"
        );
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
    fn history_and_lock_hard_links_are_rejected_before_writing_or_chmod() {
        let dir = tempfile::tempdir().expect("tempdir");
        let history_victim = dir.path().join("history-victim");
        let history_path = dir.path().join("history");
        fs::write(&history_victim, "keep history\n").expect("history victim");
        fs::set_permissions(&history_victim, fs::Permissions::from_mode(0o640))
            .expect("history victim mode");
        fs::hard_link(&history_victim, &history_path).expect("history hard link");

        let mut history = load_test_history(&history_path, 10);
        assert!(history.is_empty());
        history.add("secret command");
        history.save();
        assert_eq!(
            fs::read_to_string(&history_victim).expect("history victim contents"),
            "keep history\n"
        );
        assert_eq!(
            fs::metadata(&history_victim)
                .expect("history victim metadata")
                .permissions()
                .mode()
                & 0o777,
            0o640
        );

        let lock_victim = dir.path().join("lock-victim");
        let separate_history = dir.path().join("separate-history");
        let lock_path = dir.path().join("separate-history.lock");
        fs::write(&lock_victim, "keep lock\n").expect("lock victim");
        fs::set_permissions(&lock_victim, fs::Permissions::from_mode(0o640))
            .expect("lock victim mode");
        fs::hard_link(&lock_victim, &lock_path).expect("lock hard link");

        assert!(lock_history_file(&separate_history).is_err());
        assert_eq!(
            fs::read_to_string(&lock_victim).expect("lock victim contents"),
            "keep lock\n"
        );
        assert_eq!(
            fs::metadata(&lock_victim)
                .expect("lock victim metadata")
                .permissions()
                .mode()
                & 0o777,
            0o640
        );
    }

    #[test]
    fn history_loader_rejects_a_fifo_without_blocking() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("history");
        mkfifo(&path, Mode::S_IRUSR | Mode::S_IWUSR).expect("history fifo");

        let history = load_test_history(&path, 10);

        assert!(history.is_empty());
    }

    #[test]
    fn history_lock_descriptor_closes_across_exec() {
        let dir = tempfile::tempdir().expect("tempdir");
        fs::set_permissions(dir.path(), fs::Permissions::from_mode(0o700)).expect("private parent");
        let path = dir.path().join("history");
        let lock = lock_history_file(&path).expect("history lock");

        let flags = unsafe { nix::libc::fcntl(lock.file.as_raw_fd(), nix::libc::F_GETFD) };

        assert!(flags >= 0, "F_GETFD failed");
        assert_ne!(flags & nix::libc::FD_CLOEXEC, 0);
        let directory_flags =
            unsafe { nix::libc::fcntl(lock._directory.as_raw_fd(), nix::libc::F_GETFD) };
        assert!(directory_flags >= 0, "directory F_GETFD failed");
        assert_ne!(directory_flags & nix::libc::FD_CLOEXEC, 0);
    }

    #[test]
    fn history_load_and_save_refuse_an_oversized_file_without_replacing_it() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("history");
        let file = OpenOptions::new()
            .write(true)
            .create_new(true)
            .mode(0o600)
            .open(&path)
            .expect("history fixture");
        file.set_len((MAX_HISTORY_FILE_BYTES + 1) as u64)
            .expect("sparse oversized fixture");
        drop(file);

        let mut history = load_test_history(&path, 10);
        assert!(history.is_empty());
        history.add("echo must not replace oversized history");
        history.save();

        assert_eq!(
            fs::metadata(&path).expect("history metadata").len(),
            (MAX_HISTORY_FILE_BYTES + 1) as u64
        );
    }

    #[test]
    fn oversized_history_records_are_not_retained_or_persisted() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("history");
        let mut history = load_test_history(&path, 10);

        history.add(&"x".repeat(MAX_HISTORY_RECORD_BYTES));

        assert!(history.is_empty());
        assert!(!path.exists());
    }

    #[test]
    fn terminal_ambiguous_commands_are_never_loaded_or_recalled() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("history");
        let records = [
            HistoryEntry {
                command: "echo safe".into(),
                timestamp: 1,
                cwd: Some("/tmp/unsafe\nmetadata".into()),
            },
            HistoryEntry {
                command: "echo \x1b]52;c;payload\x07".into(),
                timestamp: 2,
                cwd: None,
            },
            HistoryEntry {
                command: "echo \u{202e}hidden".into(),
                timestamp: 3,
                cwd: None,
            },
        ];
        let content = records
            .iter()
            .map(History::format_entry)
            .collect::<io::Result<Vec<_>>>()
            .expect("serialize")
            .join("\n");
        fs::write(&path, format!("{content}\n")).expect("fixture");

        let mut history = load_test_history(&path, 10);

        assert_eq!(history.entries(), vec!["echo safe"]);
        assert_eq!(history.entries[0].cwd, None);
        history.add("echo \u{200b}hidden");
        assert_eq!(history.entries(), vec!["echo safe"]);
    }

    #[test]
    fn loader_keeps_only_the_newest_requested_entries() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("history");
        let records = (0..5)
            .map(|index| HistoryEntry {
                command: format!("echo {index}"),
                timestamp: index,
                cwd: None,
            })
            .collect::<Vec<_>>();
        write_entries_atomically(&path, &records).expect("history fixture");

        let history = load_test_history(&path, 2);

        assert_eq!(history.entries(), vec!["echo 3", "echo 4"]);
    }

    #[test]
    fn lock_wait_is_bounded_and_lock_name_replacement_cannot_bypass_it() {
        let dir = tempfile::tempdir().expect("tempdir");
        fs::set_permissions(dir.path(), fs::Permissions::from_mode(0o700)).expect("private parent");
        let path = dir.path().join("history");
        let lock_path = dir.path().join("history.lock");
        let displaced = dir.path().join("displaced.lock");
        let first = lock_history_file(&path).expect("first lock");
        fs::rename(&lock_path, &displaced).expect("replace lock namespace");

        let error = lock_history_file_with_timeout(&path, Duration::ZERO)
            .err()
            .expect("second lock must time out");
        assert_eq!(error.kind(), io::ErrorKind::WouldBlock);
        assert!(!lock_path.exists(), "contender created a bypass lock inode");

        drop(first);
        let second = lock_history_file_with_timeout(&path, Duration::from_millis(50));
        assert!(second.is_ok(), "lock should recover after owner exits");
    }
}
