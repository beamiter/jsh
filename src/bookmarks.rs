/// Directory bookmarks: named shortcuts to directories.
use std::collections::HashMap;
use std::io;
use std::path::{Path, PathBuf};
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
        match read_bookmarks(&self.file_path) {
            Ok(bookmarks) => self.bookmarks = bookmarks,
            Err(error) => warn_bookmark_io("load", &self.file_path, &error),
        }
    }

    pub fn add(&mut self, name: &str, path: &str) -> bool {
        if !valid_bookmark_field(name) || !valid_bookmark_field(path) || name.contains('|') {
            eprintln!(
                "jsh: bookmark name/path contains unsafe terminal text or exceeds its size limit"
            );
            return false;
        }
        match self.add_inner(name, path) {
            Ok(()) => true,
            Err(error) => {
                warn_bookmark_io("save", &self.file_path, &error);
                false
            }
        }
    }

    fn add_inner(&mut self, name: &str, path: &str) -> io::Result<()> {
        let _lock = crate::io_guard::lock_private_file(&self.file_path)?;
        // Apply only this operation to the latest committed state. Merging a
        // stale snapshot would otherwise resurrect another shell's removals.
        let mut bookmarks = read_bookmarks(&self.file_path)?;
        if bookmarks.len() >= MAX_BOOKMARK_ENTRIES && !bookmarks.contains_key(name) {
            return Err(io::Error::other(
                "bookmark database reached its entry limit",
            ));
        }
        let current_bytes = bookmarks
            .iter()
            .map(|(name, path)| name.len().saturating_add(path.len()).saturating_add(2))
            .sum::<usize>();
        let replaced_bytes = bookmarks.get(name).map_or(0, |old| {
            name.len().saturating_add(old.len()).saturating_add(2)
        });
        let projected = current_bytes
            .saturating_sub(replaced_bytes)
            .saturating_add(name.len())
            .saturating_add(path.len())
            .saturating_add(2);
        if projected > MAX_BOOKMARK_FILE_BYTES {
            return Err(io::Error::other("bookmark database reached its byte limit"));
        }
        bookmarks.insert(name.to_string(), path.to_string());
        write_bookmarks(&self.file_path, &bookmarks)?;
        self.bookmarks = bookmarks;
        Ok(())
    }

    pub fn get(&self, name: &str) -> Option<&String> {
        self.bookmarks.get(name)
    }

    pub fn remove(&mut self, name: &str) -> bool {
        match self.try_remove(name) {
            Ok(removed) => removed,
            Err(error) => {
                warn_bookmark_io("remove", &self.file_path, &error);
                false
            }
        }
    }

    /// Distinguish a missing bookmark from a persistence failure for callers
    /// that report an exit status. The legacy boolean API remains available.
    pub fn try_remove(&mut self, name: &str) -> io::Result<bool> {
        let _lock = crate::io_guard::lock_private_file(&self.file_path)?;
        let mut bookmarks = read_bookmarks(&self.file_path)?;
        let removed = bookmarks.remove(name).is_some();
        if removed {
            write_bookmarks(&self.file_path, &bookmarks)?;
        }
        self.bookmarks = bookmarks;
        Ok(removed)
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

fn read_bookmarks(path: &Path) -> io::Result<HashMap<String, String>> {
    let content = match crate::io_guard::read_private_text(path, MAX_BOOKMARK_FILE_BYTES) {
        Ok(content) => content,
        Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(HashMap::new()),
        Err(error) => return Err(error),
    };
    let mut bookmarks = HashMap::new();
    for line in content.lines().take(MAX_BOOKMARK_ENTRIES) {
        // Whitespace belongs to the name/path. Trimming the complete record
        // corrupts valid directories whose names end in a space.
        if let Some((name, path)) = line.split_once('|') {
            if valid_bookmark_field(name) && valid_bookmark_field(path) {
                bookmarks.insert(name.to_string(), path.to_string());
            }
        }
    }
    Ok(bookmarks)
}

fn write_bookmarks(path: &Path, bookmarks: &HashMap<String, String>) -> io::Result<()> {
    let mut content = String::new();
    let mut entries: Vec<_> = bookmarks.iter().collect();
    entries.sort_unstable_by_key(|(name, _)| *name);
    for (name, path) in entries {
        content.push_str(&format!("{name}|{path}\n"));
    }
    crate::io_guard::write_private_file_atomic(path, content.as_bytes(), MAX_BOOKMARK_FILE_BYTES)
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

    fn database(path: &Path) -> BookmarkDB {
        let mut db = BookmarkDB {
            bookmarks: HashMap::new(),
            file_path: path.to_owned(),
        };
        db.load();
        db
    }

    #[test]
    fn independent_shells_preserve_additions_replacements_and_removals() {
        let temp = tempfile::tempdir().unwrap();
        let path = temp.path().join("bookmarks");
        let mut first = database(&path);
        let mut second = database(&path);
        assert!(first.add("first", "/one"));
        assert!(second.add("second", "/two"));
        assert_eq!(database(&path).names(), ["first", "second"]);

        // A stale instance must be able to delete a peer's new bookmark.
        assert!(first.try_remove("second").unwrap());
        assert!(second.add("third", "/three"));
        assert_eq!(database(&path).names(), ["first", "third"]);

        assert!(first.add("first", "/updated"));
        assert!(second.add("fourth", "/four"));
        assert_eq!(database(&path).get("first").unwrap(), "/updated");
        assert!(!second.try_remove("missing").unwrap());
    }

    #[test]
    fn concurrent_bookmark_writers_do_not_lose_committed_names() {
        let temp = tempfile::tempdir().unwrap();
        let path = temp.path().join("bookmarks");
        let barrier = std::sync::Barrier::new(4);
        std::thread::scope(|scope| {
            for writer in 0..4 {
                let path = &path;
                let barrier = &barrier;
                scope.spawn(move || {
                    let mut db = database(path);
                    barrier.wait();
                    // A busy runner can legitimately exhaust one acquisition's
                    // two-second budget while another writer is syncing files.
                    // Test committed data, not filesystem scheduling: retry
                    // only contention, with one bounded budget for this writer.
                    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(15);
                    for item in 0..8 {
                        let name = format!("writer-{writer}-{item}");
                        loop {
                            match db.add_inner(&name, "/shared") {
                                Ok(()) => break,
                                Err(error)
                                    if error.kind() == io::ErrorKind::WouldBlock
                                        && std::time::Instant::now() < deadline =>
                                {
                                    std::thread::sleep(std::time::Duration::from_millis(5));
                                }
                                Err(error) => panic!("writer {writer}, item {item}: {error:?}"),
                            }
                        }
                    }
                });
            }
        });
        let expected: Vec<_> = (0..4)
            .flat_map(|writer| (0..8).map(move |item| format!("writer-{writer}-{item}")))
            .collect();
        assert_eq!(database(&path).names(), expected);
    }

    #[test]
    fn bookmark_roundtrip_preserves_spaces_and_delimiters_in_paths() {
        let temp = tempfile::tempdir().unwrap();
        let path = temp.path().join("bookmarks");
        let mut db = database(&path);
        assert!(db.add(" project ", "/tmp/project | notes "));
        assert_eq!(
            database(&path).get(" project ").map(String::as_str),
            Some("/tmp/project | notes ")
        );
    }

    #[test]
    fn failed_bookmark_write_preserves_disk_and_in_memory_state() {
        let temp = tempfile::tempdir().unwrap();
        let path = temp.path().join("bookmarks");
        let mut db = database(&path);
        assert!(db.add("existing", "/original"));
        let contents = std::fs::read(&path).unwrap();
        let lock = temp.path().join("bookmarks.lock");
        std::fs::remove_file(&lock).unwrap();
        let target = temp.path().join("outside");
        std::fs::write(&target, b"unchanged").unwrap();
        std::os::unix::fs::symlink(&target, &lock).unwrap();
        assert!(!db.add("new", "/new"));
        assert!(db.try_remove("existing").is_err());
        assert_eq!(db.get("existing").map(String::as_str), Some("/original"));
        assert!(db.get("new").is_none());
        assert_eq!(std::fs::read(&path).unwrap(), contents);
        assert_eq!(std::fs::read(&target).unwrap(), b"unchanged");
    }

    #[test]
    fn unreadable_bookmark_data_is_not_replaced_with_a_partial_snapshot() {
        let temp = tempfile::tempdir().unwrap();
        let path = temp.path().join("bookmarks");
        let mut db = database(&path);
        assert!(db.add("existing", "/original"));
        // An invalid UTF-8 file cannot be safely merged.
        std::fs::write(&path, [0xff, 0xfe]).unwrap();
        assert!(!db.add("new", "/new"));
        assert_eq!(std::fs::read(&path).unwrap(), [0xff, 0xfe]);
        assert_eq!(db.names(), ["existing"]);
    }

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
