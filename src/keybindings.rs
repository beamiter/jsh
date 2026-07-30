/// Advanced keybindings and search enhancements for jsh editor
/// Supports customizable Vi/Emacs modes and quick navigation
use std::collections::HashMap;

/// Configurable key binding
#[derive(Debug, Clone)]
pub struct KeyBinding {
    pub keys: String,
    pub action: String,
    pub description: String,
}

/// Key binding manager
pub struct KeyBindingManager {
    bindings: HashMap<String, String>,
    mode: EditorMode,
}

#[derive(Debug, Clone, Copy, PartialEq)]
pub enum EditorMode {
    Vi,
    Emacs,
}

impl KeyBindingManager {
    pub fn new(mode: EditorMode) -> Self {
        let mut mgr = KeyBindingManager {
            bindings: HashMap::new(),
            mode,
        };
        mgr.init_defaults();
        mgr
    }

    fn init_defaults(&mut self) {
        match self.mode {
            EditorMode::Emacs => {
                // Emacs mode bindings
                self.bindings
                    .insert("C-a".to_string(), "move_home".to_string());
                self.bindings
                    .insert("C-e".to_string(), "move_end".to_string());
                self.bindings
                    .insert("C-b".to_string(), "move_left".to_string());
                self.bindings
                    .insert("C-f".to_string(), "move_right".to_string());
                self.bindings
                    .insert("C-p".to_string(), "history_prev".to_string());
                self.bindings
                    .insert("C-n".to_string(), "history_next".to_string());
                self.bindings
                    .insert("C-r".to_string(), "search_history".to_string());
                self.bindings
                    .insert("C-s".to_string(), "search_forward".to_string());
                self.bindings
                    .insert("C-u".to_string(), "delete_line".to_string());
                self.bindings
                    .insert("C-k".to_string(), "kill_line".to_string());
                self.bindings
                    .insert("M-f".to_string(), "move_word_right".to_string());
                self.bindings
                    .insert("M-b".to_string(), "move_word_left".to_string());
                self.bindings
                    .insert("M-d".to_string(), "kill_word".to_string());
            }
            EditorMode::Vi => {
                // Vi mode bindings
                self.bindings
                    .insert("h".to_string(), "move_left".to_string());
                self.bindings
                    .insert("j".to_string(), "history_next".to_string());
                self.bindings
                    .insert("k".to_string(), "history_prev".to_string());
                self.bindings
                    .insert("l".to_string(), "move_right".to_string());
                self.bindings
                    .insert("0".to_string(), "move_home".to_string());
                self.bindings
                    .insert("$".to_string(), "move_end".to_string());
                self.bindings
                    .insert("w".to_string(), "move_word_right".to_string());
                self.bindings
                    .insert("b".to_string(), "move_word_left".to_string());
                self.bindings
                    .insert("/".to_string(), "search_forward".to_string());
                self.bindings
                    .insert("?".to_string(), "search_backward".to_string());
                self.bindings
                    .insert("n".to_string(), "next_search_result".to_string());
                self.bindings
                    .insert("N".to_string(), "prev_search_result".to_string());
            }
        }
    }

    /// Get action for key combination
    pub fn get_action(&self, key: &str) -> Option<&str> {
        self.bindings.get(key).map(|s| s.as_str())
    }

    /// Set custom keybinding
    pub fn set_binding(&mut self, keys: String, action: String) {
        self.bindings.insert(keys, action);
    }

    /// Remove keybinding
    pub fn remove_binding(&mut self, keys: &str) {
        self.bindings.remove(keys);
    }

    /// List all bindings
    pub fn list_bindings(&self) -> Vec<(String, String)> {
        self.bindings
            .iter()
            .map(|(k, v)| (k.clone(), v.clone()))
            .collect()
    }

    /// Change editor mode
    pub fn set_mode(&mut self, mode: EditorMode) {
        if self.mode != mode {
            self.mode = mode;
            self.bindings.clear();
            self.init_defaults();
        }
    }
}

/// Enhanced search with regex support
pub struct SearchEngine {
    pub pattern: String,
    pub regex: bool,
    pub case_sensitive: bool,
    pub whole_word: bool,
}

impl SearchEngine {
    pub fn new() -> Self {
        SearchEngine {
            pattern: String::new(),
            regex: false,
            case_sensitive: false,
            whole_word: false,
        }
    }

    /// Search in text with current settings
    pub fn search(&self, text: &str) -> Vec<(usize, usize)> {
        let mut results = Vec::new();

        if self.pattern.is_empty() {
            return results;
        }

        let pattern = if self.case_sensitive {
            self.pattern.clone()
        } else {
            self.pattern.to_lowercase()
        };

        let search_text = if self.case_sensitive {
            text.to_string()
        } else {
            text.to_lowercase()
        };

        if self.regex {
            // Try to use regex
            if let Ok(re) = regex::Regex::new(&pattern) {
                for mat in re.find_iter(&search_text) {
                    results.push((mat.start(), mat.end()));
                }
            }
        } else if self.whole_word {
            // Search whole words
            for (i, _) in search_text.match_indices(&pattern) {
                let start = i;
                let end = i + pattern.len();

                let before_ok =
                    start == 0 || !search_text[..start].ends_with(|c: char| c.is_alphanumeric());
                let after_ok = end >= search_text.len()
                    || !search_text[end..].starts_with(|c: char| c.is_alphanumeric());

                if before_ok && after_ok {
                    results.push((start, end));
                }
            }
        } else {
            // Simple substring search
            for (i, _) in search_text.match_indices(&pattern) {
                results.push((i, i + pattern.len()));
            }
        }

        results
    }

    /// Toggle regex mode
    pub fn toggle_regex(&mut self) {
        self.regex = !self.regex;
    }

    /// Toggle case sensitive
    pub fn toggle_case_sensitive(&mut self) {
        self.case_sensitive = !self.case_sensitive;
    }

    /// Toggle whole word search
    pub fn toggle_whole_word(&mut self) {
        self.whole_word = !self.whole_word;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_keybinding_manager() {
        let mut mgr = KeyBindingManager::new(EditorMode::Emacs);
        assert_eq!(mgr.get_action("C-a"), Some("move_home"));

        mgr.set_binding("C-x".to_string(), "custom_action".to_string());
        assert_eq!(mgr.get_action("C-x"), Some("custom_action"));
    }

    #[test]
    fn test_search_engine() {
        let mut engine = SearchEngine::new();
        engine.pattern = "hello".to_string();

        let results = engine.search("hello world hello");
        assert_eq!(results.len(), 2);
    }

    #[test]
    fn test_search_case_insensitive() {
        let mut engine = SearchEngine::new();
        engine.pattern = "hello".to_string();
        engine.case_sensitive = false;

        let results = engine.search("Hello HELLO hello");
        assert_eq!(results.len(), 3);
    }
}
