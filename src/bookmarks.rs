/// Directory bookmarks: named shortcuts to directories.
use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::{Mutex, OnceLock};

pub struct BookmarkDB {
    bookmarks: HashMap<String, String>,
    file_path: PathBuf,
}

impl BookmarkDB {
    pub fn load_default() -> Self {
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
        if let Ok(content) = std::fs::read_to_string(&self.file_path) {
            for line in content.lines() {
                let line = line.trim();
                if line.is_empty() {
                    continue;
                }
                if let Some((name, path)) = line.split_once('|') {
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
        std::fs::write(&self.file_path, content).ok();
    }

    pub fn add(&mut self, name: &str, path: &str) {
        self.bookmarks.insert(name.to_string(), path.to_string());
        self.save();
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

static BOOKMARK_DB: OnceLock<Mutex<BookmarkDB>> = OnceLock::new();

pub fn get_bookmark_db() -> &'static Mutex<BookmarkDB> {
    BOOKMARK_DB.get_or_init(|| Mutex::new(BookmarkDB::load_default()))
}
