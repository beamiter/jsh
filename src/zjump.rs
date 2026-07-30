/// Z-jump: frecency-based directory jumping.
use std::path::PathBuf;
use std::time::{SystemTime, UNIX_EPOCH};

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
        if let Ok(content) = std::fs::read_to_string(&self.file_path) {
            for line in content.lines() {
                let parts: Vec<&str> = line.splitn(3, '|').collect();
                if parts.len() == 3 {
                    if let (Ok(rank), Ok(ts)) = (parts[1].parse::<f64>(), parts[2].parse::<u64>()) {
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
        std::fs::write(&self.file_path, content).ok();
    }

    fn now() -> u64 {
        SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map(|d| d.as_secs())
            .unwrap_or(0)
    }

    pub fn add(&mut self, path: &str) {
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
        if self.entries.len() > 200 {
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
