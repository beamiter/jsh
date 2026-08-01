/// Z-jump: frecency-based directory jumping.
use std::path::PathBuf;
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
        let content = match crate::io_guard::read_private_text(&self.file_path, MAX_Z_FILE_BYTES) {
            Ok(content) => content,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => return,
            Err(error) => {
                warn_z_io("load", &self.file_path, &error);
                return;
            }
        };
        for line in content.lines().take(MAX_Z_ENTRIES) {
            let parts: Vec<&str> = line.splitn(3, '|').collect();
            if parts.len() == 3 && valid_z_path(parts[0]) {
                if let (Ok(rank), Ok(ts)) = (parts[1].parse::<f64>(), parts[2].parse::<u64>()) {
                    if rank.is_finite() && rank > 0.0 {
                        self.entries.push(ZEntry {
                            path: parts[0].to_string(),
                            rank,
                            last_access: ts,
                        });
                    }
                }
            }
        }
    }

    pub fn save(&self) {
        let mut content = String::new();
        for entry in &self.entries {
            content.push_str(&format!(
                "{}|{}|{}\n",
                entry.path, entry.rank, entry.last_access
            ));
        }
        if let Err(error) = crate::io_guard::write_private_file_atomic(
            &self.file_path,
            content.as_bytes(),
            MAX_Z_FILE_BYTES,
        ) {
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
        let now = Self::now();
        if let Some(entry) = self.entries.iter_mut().find(|e| e.path == path) {
            entry.rank += 1.0;
            entry.last_access = now;
        } else {
            self.entries.push(ZEntry {
                path: path.to_string(),
                rank: 1.0,
                last_access: now,
            });
        }
        // Prune entries with very low frecency (keep top 100)
        if self.entries.len() > MAX_Z_ENTRIES {
            let now = Self::now();
            self.entries.sort_by(|a, b| {
                frecency(b, now)
                    .partial_cmp(&frecency(a, now))
                    .unwrap_or(std::cmp::Ordering::Equal)
            });
            self.entries.truncate(100);
        }
        self.save();
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
        self.entries.retain(|e| e.path != path);
        self.save();
    }
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
