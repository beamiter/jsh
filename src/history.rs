/// History management: file I/O, in-memory ring, search.
/// Supports timestamped entries for rich Ctrl+R panel display.
use nix::fcntl::{Flock, FlockArg};
use serde::{Deserialize, Serialize};
use std::collections::HashSet;
use std::fs::{self, File, OpenOptions};
use std::io::{self, BufRead, IsTerminal, Write};
use std::os::unix::fs::{OpenOptionsExt, PermissionsExt};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::time::{SystemTime, UNIX_EPOCH};

const HISTORY_RECORD_VERSION: u32 = 1;
static TEMP_FILE_COUNTER: AtomicU64 = AtomicU64::new(0);
/// One hint per process, however many `History` instances get built.
static LEGACY_HISTORY_HINT_SHOWN: AtomicBool = AtomicBool::new(false);

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

        let home = dirs::home_dir().unwrap_or_else(|| PathBuf::from("/tmp"));
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
        let Ok(legacy_entries) = read_entries(&legacy_path) else {
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
        eprintln!(
            "jsh: {} holds {} command(s) from before the rsh->jsh rename, and {} already existed, so they were not imported automatically.",
            legacy_path.display(),
            legacy_entries.len(),
            self.file_path.display()
        );
        eprintln!(
            "jsh: import them with: cat {} >> {}",
            legacy_path.display(),
            self.file_path.display()
        );
    }

    fn new_with_path(max_size: usize, file_path: PathBuf) -> Self {
        let mut h = History {
            entries: Vec::new(),
            max_size,
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
        let _lock = self
            .file_path
            .exists()
            .then(|| lock_history_file(&self.file_path).ok())
            .flatten();
        self.entries = read_entries_or_empty(&self.file_path).unwrap_or_default();
        trim_to_limit(&mut self.entries, self.max_size);
        if self.file_path.exists() {
            let _ = set_private_file_permissions(&self.file_path);
        }
        self.position = self.entries.len();
    }

    fn parse_line(line: &str) -> Option<HistoryEntry> {
        if line.is_empty() {
            return None;
        }

        if let Ok(record) = serde_json::from_str::<HistoryRecord>(line) {
            return (record.jsh_history_version == HISTORY_RECORD_VERSION).then_some(
                HistoryEntry {
                    command: record.command,
                    timestamp: record.timestamp,
                    cwd: record.cwd,
                },
            );
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
                return Some(HistoryEntry {
                    command: parts[2].to_string(),
                    timestamp: ts,
                    cwd,
                });
            }
        }
        Some(HistoryEntry {
            command: line.to_string(),
            timestamp: 0,
            cwd: None,
        })
    }

    fn format_entry(entry: &HistoryEntry) -> io::Result<String> {
        serde_json::to_string(&HistoryRecord::from(entry)).map_err(io::Error::other)
    }

    pub fn save(&self) {
        let _ = self.save_inner();
    }

    fn save_inner(&self) -> io::Result<()> {
        let _lock = lock_history_file(&self.file_path)?;

        // Merge with what is currently on disk while holding the lock. This
        // preserves commands appended by shells launched after this instance.
        let mut merged = read_entries_or_empty(&self.file_path)?;
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
        trim_to_limit(&mut merged, self.max_size);
        write_entries_atomically(&self.file_path, &merged)
    }

    pub fn add(&mut self, entry: &str) {
        self.add_with_cwd(entry, None);
    }

    pub fn add_with_cwd(&mut self, entry: &str, cwd: Option<&str>) {
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

        let he = HistoryEntry {
            command: command.clone(),
            timestamp,
            cwd: cwd.map(|s| s.to_string()),
        };

        self.entries.push(he.clone());
        if self.entries.len() > self.max_size {
            self.entries.remove(0);
        }
        self.position = self.entries.len();

        let _ = append_entry(&self.file_path, &he);
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

    pub fn prev(&mut self) -> Option<&str> {
        if self.position > 0 {
            self.position -= 1;
            Some(&self.entries[self.position].command)
        } else {
            None
        }
    }

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
        let mut results: Vec<(String, Vec<usize>, i32, u64, Option<String>)> = Vec::new();

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

        results.sort_by(|a, b| b.2.cmp(&a.2));
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

fn trim_to_limit(entries: &mut Vec<HistoryEntry>, max_size: usize) {
    if entries.len() > max_size {
        let remove = entries.len() - max_size;
        entries.drain(..remove);
    }
}

fn read_entries(path: &Path) -> io::Result<Vec<HistoryEntry>> {
    let file = File::open(path)?;
    let reader = io::BufReader::new(file);
    Ok(reader
        .lines()
        .map_while(Result::ok)
        .filter_map(|line| History::parse_line(&line))
        .collect())
}

fn read_entries_or_empty(path: &Path) -> io::Result<Vec<HistoryEntry>> {
    match read_entries(path) {
        Ok(entries) => Ok(entries),
        Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(Vec::new()),
        Err(error) => Err(error),
    }
}

fn append_entry(path: &Path, entry: &HistoryEntry) -> io::Result<()> {
    let _lock = lock_history_file(path)?;
    let mut file = OpenOptions::new()
        .create(true)
        .append(true)
        .mode(0o600)
        .open(path)?;
    set_private_file_permissions(path)?;

    // Build the complete record first, then issue one append write. O_APPEND
    // plus the sidecar lock prevents updated jsh processes from interleaving
    // JSON records.
    let mut record = History::format_entry(entry)?.into_bytes();
    record.push(b'\n');
    file.write_all(&record)
}

fn write_entries_atomically(path: &Path, entries: &[HistoryEntry]) -> io::Result<()> {
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
            .mode(0o600)
            .open(&tmp_path)?;
        fs::set_permissions(&tmp_path, fs::Permissions::from_mode(0o600))?;
        for entry in entries {
            file.write_all(History::format_entry(entry)?.as_bytes())?;
            file.write_all(b"\n")?;
        }
        file.sync_all()?;
        fs::rename(&tmp_path, path)?;
        set_private_file_permissions(path)?;
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

fn lock_history_file(path: &Path) -> io::Result<Flock<File>> {
    if let Some(parent) = path.parent() {
        if !parent.as_os_str().is_empty() {
            fs::create_dir_all(parent)?;
        }
    }
    let mut lock_name = path.as_os_str().to_os_string();
    lock_name.push(".lock");
    let lock_path = PathBuf::from(lock_name);
    let file = OpenOptions::new()
        .read(true)
        .write(true)
        .create(true)
        .mode(0o600)
        .open(&lock_path)?;
    set_private_file_permissions(&lock_path)?;
    Flock::lock(file, FlockArg::LockExclusive)
        .map_err(|(_, errno)| io::Error::from_raw_os_error(errno as i32))
}

fn set_private_file_permissions(path: &Path) -> io::Result<()> {
    fs::set_permissions(path, fs::Permissions::from_mode(0o600))
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
    use std::os::unix::fs::PermissionsExt;

    fn load_test_history(path: &Path, max_size: usize) -> History {
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
}
