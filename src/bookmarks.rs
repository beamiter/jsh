/// Directory bookmarks: named shortcuts to directories.
use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Mutex, OnceLock};

const MAX_BOOKMARK_FILE_BYTES: usize = 2 * 1024 * 1024;
const MAX_BOOKMARK_ENTRIES: usize = 10_000;
const MAX_BOOKMARK_FIELD_BYTES: usize = 16 * 1024;
static BOOKMARK_IO_WARNING_SHOWN: AtomicBool = AtomicBool::new(false);

pub struct BookmarkDB {
    bookmarks: HashMap<String, String>,
    file_path: PathBuf,
}

impl BookmarkDB {
    pub fn load_default() -> Self {
        // Bookmarks moved with the 0.2.0 rename; copy ~/.rsh_bookmarks across
        // before the first read so `bookmark ls` is not silently empty.
        crate::config::migrate_legacy_rsh_data();

        let path = dirs::home_dir()
            .unwrap_or_else(|| PathBuf::from("/tmp"))
            .join(".jsh_bookmarks");
        let mut db = BookmarkDB {
            bookmarks: HashMap::new(),
            file_path: path,
        };
        db.load();
        db
    }

    fn load(&mut self) {
        let content =
            match crate::io_guard::read_private_text(&self.file_path, MAX_BOOKMARK_FILE_BYTES) {
                Ok(content) => content,
                Err(error) if error.kind() == std::io::ErrorKind::NotFound => return,
                Err(error) => {
                    warn_bookmark_io("load", &self.file_path, &error);
                    return;
                }
            };
        for line in content.lines().take(MAX_BOOKMARK_ENTRIES) {
            let line = line.trim();
            if line.is_empty() {
                continue;
            }
            if let Some((name, path)) = line.split_once('|') {
                if valid_bookmark_field(name) && valid_bookmark_field(path) && !name.contains('|') {
                    self.bookmarks.insert(name.to_string(), path.to_string());
                }
            }
        }
    }

    fn save(&self) {
        let mut content = String::new();
        let mut entries: Vec<_> = self.bookmarks.iter().collect();
        entries.sort_by_key(|(k, _)| (*k).clone());
        for (name, path) in entries {
            content.push_str(&format!("{}|{}\n", name, path));
        }
        if let Err(error) = crate::io_guard::write_private_file_atomic(
            &self.file_path,
            content.as_bytes(),
            MAX_BOOKMARK_FILE_BYTES,
        ) {
            warn_bookmark_io("save", &self.file_path, &error);
        }
    }

    pub fn add(&mut self, name: &str, path: &str) -> bool {
        if !valid_bookmark_field(name) || !valid_bookmark_field(path) || name.contains('|') {
            eprintln!(
                "jsh: bookmark name/path contains unsafe terminal text or exceeds its size limit"
            );
            return false;
        }
        if self.bookmarks.len() >= MAX_BOOKMARK_ENTRIES && !self.bookmarks.contains_key(name) {
            eprintln!("jsh: bookmark database reached its entry limit");
            return false;
        }
        let current_bytes = self
            .bookmarks
            .iter()
            .map(|(name, path)| name.len().saturating_add(path.len()).saturating_add(2))
            .sum::<usize>();
        let replaced_bytes = self.bookmarks.get(name).map_or(0, |old| {
            name.len().saturating_add(old.len()).saturating_add(2)
        });
        let projected = current_bytes
            .saturating_sub(replaced_bytes)
            .saturating_add(name.len())
            .saturating_add(path.len())
            .saturating_add(2);
        if projected > MAX_BOOKMARK_FILE_BYTES {
            eprintln!("jsh: bookmark database reached its byte limit");
            return false;
        }
        self.bookmarks.insert(name.to_string(), path.to_string());
        self.save();
        true
    }

    pub fn get(&self, name: &str) -> Option<&String> {
        self.bookmarks.get(name)
    }

    pub fn remove(&mut self, name: &str) -> bool {
        let removed = self.bookmarks.remove(name).is_some();
        if removed {
            self.save();
        }
        removed
    }

    pub fn list(&self) -> Vec<(&String, &String)> {
        let mut entries: Vec<_> = self.bookmarks.iter().collect();
        entries.sort_by_key(|(k, _)| (*k).clone());
        entries
    }

    pub fn names(&self) -> Vec<String> {
        let mut names: Vec<_> = self.bookmarks.keys().cloned().collect();
        names.sort();
        names
    }
}

fn valid_bookmark_field(value: &str) -> bool {
    !value.is_empty()
        && value.len() <= MAX_BOOKMARK_FIELD_BYTES
        && crate::terminal_text::is_safe_inline(value)
}

fn warn_bookmark_io(operation: &str, path: &std::path::Path, error: &std::io::Error) {
    if !BOOKMARK_IO_WARNING_SHOWN.swap(true, Ordering::SeqCst) {
        eprintln!("jsh: bookmark {operation} failed for {path:?}: {error}");
    }
}

static BOOKMARK_DB: OnceLock<Mutex<BookmarkDB>> = OnceLock::new();

pub fn get_bookmark_db() -> &'static Mutex<BookmarkDB> {
    BOOKMARK_DB.get_or_init(|| Mutex::new(BookmarkDB::load_default()))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::os::unix::fs::PermissionsExt;

    #[test]
    fn unsafe_or_oversized_bookmark_records_are_not_loaded() {
        let temp = tempfile::tempdir().unwrap();
        std::fs::set_permissions(temp.path(), std::fs::Permissions::from_mode(0o700)).unwrap();
        let path = temp.path().join("bookmarks");
        crate::io_guard::write_private_file_atomic(
            &path,
            "ok|/tmp\nbad\u{202e}|/tmp\nline|/tmp\u{00ad}\n".as_bytes(),
            MAX_BOOKMARK_FILE_BYTES,
        )
        .unwrap();
        let mut db = BookmarkDB {
            bookmarks: HashMap::new(),
            file_path: path,
        };
        db.load();
        assert_eq!(db.get("ok").map(String::as_str), Some("/tmp"));
        assert!(db.get("bad\u{202e}").is_none());
        assert!(db.get("line").is_none());
    }
}
