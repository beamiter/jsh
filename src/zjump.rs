/// Z-jump: frecency-based directory jumping.
use std::io;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{SystemTime, UNIX_EPOCH};

const MAX_Z_FILE_BYTES: usize = 2 * 1024 * 1024;
const MAX_Z_ENTRIES: usize = 200;
const MAX_Z_PATH_BYTES: usize = 16 * 1024;
static Z_IO_WARNING_SHOWN: AtomicBool = AtomicBool::new(false);

#[derive(Debug, Clone)]
struct ZEntry {
    path: String,
    rank: f64,
    last_access: u64,
}

pub struct ZDatabase {
    entries: Vec<ZEntry>,
    file_path: PathBuf,
}

impl ZDatabase {
    pub fn load_default() -> Self {
        crate::config::migrate_legacy_rsh_data();
        let path = dirs::home_dir()
            .unwrap_or_else(|| PathBuf::from("/tmp"))
            .join(".jsh_z");
        let mut db = ZDatabase {
            entries: Vec::new(),
            file_path: path,
        };
        db.load();
        db
    }

    fn load(&mut self) {
        match read_z_entries(&self.file_path) {
            Ok(entries) => self.entries = entries,
            Err(error) => warn_z_io("load", &self.file_path, &error),
        }
    }

    /// Mutations are committed immediately. An explicit save rewrites the
    /// latest committed state, never a potentially stale in-memory snapshot.
    pub fn save(&self) {
        let result = (|| {
            let _lock = crate::io_guard::lock_private_file(&self.file_path)?;
            let entries = read_z_entries(&self.file_path)?;
            write_z_entries(&self.file_path, &entries)
        })();
        if let Err(error) = result {
            warn_z_io("save", &self.file_path, &error);
        }
    }

    fn now() -> u64 {
        SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map(|d| d.as_secs())
            .unwrap_or(0)
    }

    pub fn add(&mut self, path: &str) {
        if !valid_z_path(path) {
            return;
        }
        let result = self.update_entries(|entries| {
            let now = Self::now();
            if let Some(entry) = entries.iter_mut().find(|e| e.path == path) {
                entry.rank += 1.0;
                entry.last_access = now;
            } else {
                entries.push(ZEntry {
                    path: path.to_string(),
                    rank: 1.0,
                    last_access: now,
                });
            }
            // Prune entries with very low frecency (keep top 100).
            if entries.len() > MAX_Z_ENTRIES {
                entries.sort_by(|a, b| {
                    frecency(b, now)
                        .total_cmp(&frecency(a, now))
                        .then_with(|| a.path.cmp(&b.path))
                });
                entries.truncate(100);
            }
        });
        if let Err(error) = result {
            warn_z_io("save", &self.file_path, &error);
        }
    }

    fn update_entries(&mut self, update: impl FnOnce(&mut Vec<ZEntry>)) -> io::Result<()> {
        let _lock = crate::io_guard::lock_private_file(&self.file_path)?;
        let mut entries = read_z_entries(&self.file_path)?;
        update(&mut entries);
        write_z_entries(&self.file_path, &entries)?;
        self.entries = entries;
        Ok(())
    }

    pub fn query(&self, keywords: &[&str]) -> Option<String> {
        let now = Self::now();
        let cwd = std::env::current_dir()
            .ok()
            .map(|p| p.to_string_lossy().to_string());

        let mut best: Option<(&ZEntry, f64)> = None;
        for entry in &self.entries {
            // Skip current directory
            if let Some(ref cwd) = cwd {
                if entry.path == *cwd {
                    continue;
                }
            }
            // All keywords must be substrings of path (case-insensitive)
            let path_lower = entry.path.to_lowercase();
            let matches = keywords
                .iter()
                .all(|kw| path_lower.contains(&kw.to_lowercase()));
            if !matches {
                continue;
            }

            let score = frecency(entry, now);
            if best.is_none() || score > best.unwrap().1 {
                best = Some((entry, score));
            }
        }
        best.map(|(e, _)| e.path.clone())
    }

    pub fn list(&self) -> Vec<(String, f64)> {
        let now = Self::now();
        let mut entries: Vec<_> = self
            .entries
            .iter()
            .map(|e| (e.path.clone(), frecency(e, now)))
            .collect();
        entries.sort_by(|a, b| b.1.partial_cmp(&a.1).unwrap_or(std::cmp::Ordering::Equal));
        entries
    }

    pub fn remove(&mut self, path: &str) {
        if let Err(error) = self.try_remove(path) {
            warn_z_io("remove", &self.file_path, &error);
        }
    }

    /// Remove from the current on-disk state and expose persistence errors to
    /// command-line callers. The original best-effort API remains available.
    pub fn try_remove(&mut self, path: &str) -> io::Result<()> {
        self.update_entries(|entries| entries.retain(|entry| entry.path != path))
    }
}

fn read_z_entries(path: &Path) -> io::Result<Vec<ZEntry>> {
    let content = match crate::io_guard::read_private_text(path, MAX_Z_FILE_BYTES) {
        Ok(content) => content,
        Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(Vec::new()),
        Err(error) => return Err(error),
    };
    let mut entries = Vec::new();
    for line in content.lines().take(MAX_Z_ENTRIES) {
        let mut parts = line.splitn(3, '|');
        if let (Some(path), Some(rank), Some(timestamp)) =
            (parts.next(), parts.next(), parts.next())
        {
            if let (Ok(rank), Ok(last_access)) = (rank.parse::<f64>(), timestamp.parse::<u64>()) {
                if valid_z_path(path) && rank.is_finite() && rank > 0.0 {
                    entries.push(ZEntry {
                        path: path.to_string(),
                        rank,
                        last_access,
                    });
                }
            }
        }
    }
    Ok(entries)
}

fn write_z_entries(path: &Path, entries: &[ZEntry]) -> io::Result<()> {
    let mut content = String::new();
    for entry in entries {
        content.push_str(&format!(
            "{}|{}|{}\n",
            entry.path, entry.rank, entry.last_access
        ));
    }
    crate::io_guard::write_private_file_atomic(path, content.as_bytes(), MAX_Z_FILE_BYTES)
}

fn valid_z_path(path: &str) -> bool {
    !path.is_empty()
        && path.len() <= MAX_Z_PATH_BYTES
        && !path.contains('|')
        && crate::terminal_text::is_safe_inline(path)
}

fn warn_z_io(operation: &str, path: &std::path::Path, error: &std::io::Error) {
    if !Z_IO_WARNING_SHOWN.swap(true, Ordering::SeqCst) {
        eprintln!("jsh: z-jump {operation} failed for {path:?}: {error}");
    }
}

fn frecency(entry: &ZEntry, now: u64) -> f64 {
    let age_secs = now.saturating_sub(entry.last_access);
    let weight = if age_secs < 3600 {
        4.0 // < 1 hour
    } else if age_secs < 86400 {
        2.0 // < 1 day
    } else if age_secs < 604800 {
        1.0 // < 1 week
    } else {
        0.5
    };
    entry.rank * weight
}

use std::sync::Mutex;
use std::sync::OnceLock;

static Z_DB: OnceLock<Mutex<ZDatabase>> = OnceLock::new();

pub fn get_z_db() -> &'static Mutex<ZDatabase> {
    Z_DB.get_or_init(|| Mutex::new(ZDatabase::load_default()))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::os::unix::fs::PermissionsExt;

    fn database(path: &Path) -> ZDatabase {
        let mut db = ZDatabase {
            entries: Vec::new(),
            file_path: path.to_owned(),
        };
        db.load();
        db
    }

    fn rank(db: &ZDatabase, path: &str) -> Option<f64> {
        db.entries
            .iter()
            .find(|entry| entry.path == path)
            .map(|entry| entry.rank)
    }

    #[test]
    fn independent_shells_preserve_visits_and_accumulate_shared_ranks() {
        let temp = tempfile::tempdir().unwrap();
        let path = temp.path().join("z");
        let mut first = database(&path);
        let mut second = database(&path);
        first.add("/one");
        second.add("/two");
        first.add("/one");
        second.add("/one");
        let restored = database(&path);
        assert_eq!(rank(&restored, "/one"), Some(3.0));
        assert_eq!(rank(&restored, "/two"), Some(1.0));
    }

    #[test]
    fn removed_directories_are_not_resurrected_by_stale_writers_or_save() {
        let temp = tempfile::tempdir().unwrap();
        let path = temp.path().join("z");
        let mut first = database(&path);
        first.add("/one");
        let mut second = database(&path);
        first.try_remove("/one").unwrap();
        second.save();
        assert_eq!(rank(&database(&path), "/one"), None);
        second.add("/two");
        assert_eq!(rank(&database(&path), "/one"), None);
        assert_eq!(rank(&database(&path), "/two"), Some(1.0));
        // Returning to a removed directory begins a fresh rank.
        first.add("/one");
        assert_eq!(rank(&database(&path), "/one"), Some(1.0));
    }

    #[test]
    fn concurrent_directory_visits_are_counted_once_each() {
        let temp = tempfile::tempdir().unwrap();
        let path = temp.path().join("z");
        let barrier = std::sync::Barrier::new(4);
        std::thread::scope(|scope| {
            for _ in 0..4 {
                let path = &path;
                let barrier = &barrier;
                scope.spawn(move || {
                    let mut db = database(path);
                    barrier.wait();
                    for _ in 0..8 {
                        db.add("/shared");
                    }
                });
            }
        });
        assert_eq!(rank(&database(&path), "/shared"), Some(32.0));
    }

    #[test]
    fn failed_z_updates_preserve_the_committed_and_in_memory_state() {
        let temp = tempfile::tempdir().unwrap();
        let path = temp.path().join("z");
        let mut db = database(&path);
        db.add("/one");
        let contents = std::fs::read(&path).unwrap();
        let lock = temp.path().join("z.lock");
        std::fs::remove_file(&lock).unwrap();
        // A FIFO must fail promptly instead of hanging a directory change.
        nix::unistd::mkfifo(&lock, nix::sys::stat::Mode::S_IRUSR).unwrap();
        db.add("/two");
        assert!(db.try_remove("/one").is_err());
        assert_eq!(rank(&db, "/one"), Some(1.0));
        assert_eq!(rank(&db, "/two"), None);
        assert_eq!(std::fs::read(&path).unwrap(), contents);
    }

    #[test]
    fn oversized_z_data_is_not_replaced_by_a_stale_snapshot() {
        let temp = tempfile::tempdir().unwrap();
        let path = temp.path().join("z");
        let mut db = database(&path);
        db.add("/one");
        let file = std::fs::OpenOptions::new().write(true).open(&path).unwrap();
        file.set_len(MAX_Z_FILE_BYTES as u64 + 1).unwrap();
        db.add("/two");
        assert_eq!(
            std::fs::metadata(&path).unwrap().len(),
            MAX_Z_FILE_BYTES as u64 + 1
        );
        assert_eq!(rank(&db, "/one"), Some(1.0));
        assert_eq!(rank(&db, "/two"), None);
    }

    #[test]
    fn persisted_z_entries_are_bounded_and_terminal_safe() {
        let temp = tempfile::tempdir().unwrap();
        std::fs::set_permissions(temp.path(), std::fs::Permissions::from_mode(0o700)).unwrap();
        let path = temp.path().join("z");
        crate::io_guard::write_private_file_atomic(
            &path,
            "/safe|2|10\n/hidden\u{202e}|3|11\n/nan|NaN|12\n".as_bytes(),
            MAX_Z_FILE_BYTES,
        )
        .unwrap();
        let mut db = ZDatabase {
            entries: Vec::new(),
            file_path: path,
        };
        db.load();
        assert_eq!(db.entries.len(), 1);
        assert_eq!(db.entries[0].path, "/safe");
    }
}
