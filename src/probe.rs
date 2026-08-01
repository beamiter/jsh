/// Filesystem-aware completion probe: context-aware suggestions based on command type.
/// This module provides intelligent completion by understanding what kind of filesystem
/// entries different commands prefer (files vs directories).
use std::fs;
use std::path::{Path, PathBuf};

/// What kind of filesystem entries a command prefers as arguments.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ArgPreference {
    /// Command operates only on directories (cd, mkdir, rmdir, z)
    DirectoriesOnly,
    /// Command primarily operates on regular files (cat, vim, less, head, etc.)
    FilesPreferred,
    /// Command operates on both but prefers directories when ambiguous (ls, find, tree)
    DirectoriesPreferred,
    /// Command operates on both files and directories equally (cp, mv, rm, chmod)
    Any,
    /// Command doesn't take filesystem arguments (echo, export, alias, true, etc.)
    None,
}

/// Classify a command by its argument preferences.
/// This determines what kind of filesystem entries to prioritize in suggestions.
pub fn classify_command(cmd: &str) -> ArgPreference {
    match cmd {
        // Directory-only commands
        "cd" | "pushd" | "popd" | "rmdir" | "z" | "mkdir" => ArgPreference::DirectoriesOnly,

        // File-preferred commands (editors, viewers, compilers, processors)
        "cat" | "less" | "more" | "head" | "tail" | "wc" | "file" | "stat" | "vim" | "nvim"
        | "vi" | "nano" | "emacs" | "code" | "subl" | "bat" | "diff" | "patch" | "sort"
        | "uniq" | "cut" | "paste" | "awk" | "sed" | "grep" | "egrep" | "fgrep" | "python"
        | "python3" | "ruby" | "node" | "perl" | "php" | "rustc" | "gcc" | "g++" | "clang"
        | "clang++" | "javac" | "source" | "." | "chmod" | "chown" | "chgrp" | "md5sum"
        | "sha256sum" | "sha1sum" | "shasum" | "strings" | "hexdump" | "xxd" | "od"
        | "shellcheck" | "cargo" | "make" => ArgPreference::FilesPreferred,

        // Directory-preferred commands (listing, searching, navigation)
        "ls" | "ll" | "la" | "tree" | "find" | "du" | "df" | "exa" | "eza" | "lsd" => {
            ArgPreference::DirectoriesPreferred
        }

        // Any (operates on files and directories equally)
        "cp" | "mv" | "rm" | "ln" | "tar" | "zip" | "unzip" | "gzip" | "gunzip" | "bzip2"
        | "bunzip2" | "xz" | "unxz" | "rsync" | "scp" | "touch" => ArgPreference::Any,

        // No filesystem arguments
        "echo" | "printf" | "export" | "unset" | "alias" | "unalias" | "true" | "false"
        | "exit" | "return" | "break" | "continue" | "jobs" | "fg" | "bg" | "history" | "help"
        | "set" | "declare" | "local" | "type" | "which" | "kill" | "sleep" | "wait" | "read"
        | "builtin" | "command" | "enable" | "eval" | "exec" | "test" | "[" | "shopt"
        | "ulimit" | "umask" => ArgPreference::None,

        // Unknown commands: default to Any (allows both files and dirs)
        _ => ArgPreference::Any,
    }
}

/// Probe the filesystem for the best single completion suggestion.
/// Returns the full argument text (e.g., "funny/" or "cool"), not just the suffix.
///
/// # Parameters
/// - `cmd`: The command being typed (e.g., "cat", "cd")
/// - `partial`: The partial argument typed so far (e.g., "co" in "cat co")
/// - `cwd`: Current working directory (for resolving relative paths)
///
/// # Returns
/// The best matching filesystem entry as a complete argument string, or None if no match.
pub fn probe_filesystem(cmd: &str, partial: &str, cwd: &Path) -> Option<String> {
    if partial.is_empty() {
        return None; // Don't probe with no input at all
    }

    let preference = classify_command(cmd);
    if preference == ArgPreference::None {
        return None; // Command doesn't take filesystem arguments
    }

    // Determine which directory to scan and what prefix to match
    let (scan_dir, file_prefix) = resolve_scan_target(partial, cwd);

    // Read directory entries and filter
    let entries = fs::read_dir(&scan_dir).ok()?;
    let mut candidates: Vec<(String, bool, u32)> = Vec::new(); // (name, is_dir, priority)

    for entry in entries.flatten() {
        let name = entry.file_name().to_string_lossy().to_string();

        // Skip hidden files unless prefix starts with '.'
        if name.starts_with('.') && !file_prefix.starts_with('.') {
            continue;
        }

        // Must match the prefix (case-sensitive)
        if !name.starts_with(file_prefix) {
            continue;
        }

        let is_dir = entry.file_type().map(|t| t.is_dir()).unwrap_or(false);

        // Filter and assign priority by preference
        let priority = match preference {
            ArgPreference::DirectoriesOnly => {
                if !is_dir {
                    continue; // Skip non-directories
                }
                0
            }
            ArgPreference::FilesPreferred => {
                if is_dir {
                    2 // Deprioritize directories
                } else {
                    0 // Prefer files
                }
            }
            ArgPreference::DirectoriesPreferred => {
                if is_dir {
                    0 // Prefer directories
                } else {
                    2 // Deprioritize files
                }
            }
            ArgPreference::Any => 1, // Equal priority
            ArgPreference::None => unreachable!(),
        };

        candidates.push((name, is_dir, priority));
    }

    if candidates.is_empty() {
        return None;
    }

    // Rank candidates: lower priority number = better, then shorter name, then alphabetical
    candidates.sort_by(|a, b| {
        a.2.cmp(&b.2) // Priority first
            .then_with(|| a.0.len().cmp(&b.0.len())) // Shorter names second
            .then_with(|| a.0.cmp(&b.0)) // Alphabetical third
    });

    let (best_name, best_is_dir, _) = &candidates[0];

    // Reconstruct the full path argument as the user would type it
    let result = reconstruct_path(partial, file_prefix, best_name, *best_is_dir);
    Some(result)
}

/// Parse a partial path to determine the directory to scan and the prefix to match.
///
/// Examples:
/// - "co" → (cwd, "co")
/// - "dir/co" → (cwd/dir/, "co")
/// - "/tmp/co" → (/tmp/, "co")
/// - "~/co" → (home_dir/, "co")
fn resolve_scan_target<'a>(partial: &'a str, cwd: &Path) -> (PathBuf, &'a str) {
    if partial.contains('/') {
        let last_slash = partial.rfind('/').unwrap();
        let dir_part = &partial[..=last_slash];
        let file_prefix = &partial[last_slash + 1..];

        let scan_dir = if dir_part.starts_with('/') {
            // Absolute path
            PathBuf::from(dir_part)
        } else if let Some(home_relative) = dir_part.strip_prefix("~/") {
            // Home directory
            dirs::home_dir()
                .unwrap_or_else(|| PathBuf::from("/"))
                .join(home_relative)
        } else {
            // Relative path
            cwd.join(dir_part)
        };
        (scan_dir, file_prefix)
    } else {
        // No slash: scan current directory
        (cwd.to_path_buf(), partial)
    }
}

/// Reconstruct the full path argument from the parsed components.
///
/// Examples:
/// - partial="co", file_prefix="co", best_name="cool", is_dir=false → "cool"
/// - partial="co", file_prefix="co", best_name="configs", is_dir=true → "configs/"
/// - partial="dir/co", file_prefix="co", best_name="cool", is_dir=false → "dir/cool"
fn reconstruct_path(partial: &str, _file_prefix: &str, best_name: &str, is_dir: bool) -> String {
    let base = if partial.contains('/') {
        let last_slash = partial.rfind('/').unwrap();
        &partial[..=last_slash]
    } else {
        ""
    };

    if is_dir {
        format!("{}{}/", base, best_name)
    } else {
        format!("{}{}", base, best_name)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;
    use tempfile::TempDir;

    #[test]
    fn test_classify_commands() {
        // Directory-only
        assert_eq!(classify_command("cd"), ArgPreference::DirectoriesOnly);
        assert_eq!(classify_command("mkdir"), ArgPreference::DirectoriesOnly);
        assert_eq!(classify_command("rmdir"), ArgPreference::DirectoriesOnly);
        assert_eq!(classify_command("z"), ArgPreference::DirectoriesOnly);

        // File-preferred
        assert_eq!(classify_command("cat"), ArgPreference::FilesPreferred);
        assert_eq!(classify_command("vim"), ArgPreference::FilesPreferred);
        assert_eq!(classify_command("python"), ArgPreference::FilesPreferred);
        assert_eq!(classify_command("rustc"), ArgPreference::FilesPreferred);

        // Directory-preferred
        assert_eq!(classify_command("ls"), ArgPreference::DirectoriesPreferred);
        assert_eq!(
            classify_command("tree"),
            ArgPreference::DirectoriesPreferred
        );
        assert_eq!(
            classify_command("find"),
            ArgPreference::DirectoriesPreferred
        );

        // Any
        assert_eq!(classify_command("cp"), ArgPreference::Any);
        assert_eq!(classify_command("mv"), ArgPreference::Any);
        assert_eq!(classify_command("rm"), ArgPreference::Any);

        // None
        assert_eq!(classify_command("echo"), ArgPreference::None);
        assert_eq!(classify_command("export"), ArgPreference::None);
        assert_eq!(classify_command("alias"), ArgPreference::None);

        // Unknown defaults to Any
        assert_eq!(classify_command("unknowncommand"), ArgPreference::Any);
    }

    #[test]
    fn test_probe_cd_only_shows_dirs() {
        let tmp = TempDir::new().unwrap();
        fs::create_dir(tmp.path().join("documents")).unwrap();
        fs::write(tmp.path().join("data.txt"), "").unwrap();

        let result = probe_filesystem("cd", "d", tmp.path());
        assert_eq!(result, Some("documents/".to_string()));
    }

    #[test]
    fn test_probe_cat_prefers_files() {
        let tmp = TempDir::new().unwrap();
        fs::create_dir(tmp.path().join("configs")).unwrap();
        fs::write(tmp.path().join("config.yaml"), "").unwrap();

        let result = probe_filesystem("cat", "config", tmp.path());
        assert_eq!(result, Some("config.yaml".to_string()));
    }

    #[test]
    fn test_probe_ls_prefers_dirs() {
        let tmp = TempDir::new().unwrap();
        fs::create_dir(tmp.path().join("docs")).unwrap();
        fs::write(tmp.path().join("dockerfile"), "").unwrap();

        let result = probe_filesystem("ls", "doc", tmp.path());
        assert_eq!(result, Some("docs/".to_string()));
    }

    #[test]
    fn test_probe_no_match() {
        let tmp = TempDir::new().unwrap();
        fs::write(tmp.path().join("file.txt"), "").unwrap();

        let result = probe_filesystem("cat", "xyz", tmp.path());
        assert_eq!(result, None);
    }

    #[test]
    fn test_probe_empty_partial() {
        let tmp = TempDir::new().unwrap();
        fs::write(tmp.path().join("file.txt"), "").unwrap();

        let result = probe_filesystem("cat", "", tmp.path());
        assert_eq!(result, None);
    }

    #[test]
    fn test_probe_no_filesystem_args() {
        let tmp = TempDir::new().unwrap();
        fs::write(tmp.path().join("echo.txt"), "").unwrap();

        let result = probe_filesystem("echo", "e", tmp.path());
        assert_eq!(result, None); // echo doesn't take filesystem arguments
    }

    #[test]
    fn test_probe_with_slash_in_path() {
        let tmp = TempDir::new().unwrap();
        fs::create_dir(tmp.path().join("subdir")).unwrap();
        fs::write(tmp.path().join("subdir").join("file.txt"), "").unwrap();

        let result = probe_filesystem("cat", "subdir/f", tmp.path());
        assert_eq!(result, Some("subdir/file.txt".to_string()));
    }

    #[test]
    fn test_probe_hidden_files_not_suggested_by_default() {
        let tmp = TempDir::new().unwrap();
        fs::write(tmp.path().join(".hidden"), "").unwrap();
        fs::write(tmp.path().join("visible"), "").unwrap();

        let result = probe_filesystem("cat", "h", tmp.path());
        assert_eq!(result, None); // .hidden should not match "h"
    }

    #[test]
    fn test_probe_hidden_files_with_dot_prefix() {
        let tmp = TempDir::new().unwrap();
        fs::write(tmp.path().join(".hidden"), "").unwrap();

        let result = probe_filesystem("cat", ".h", tmp.path());
        assert_eq!(result, Some(".hidden".to_string()));
    }

    #[test]
    fn test_probe_shorter_names_preferred() {
        let tmp = TempDir::new().unwrap();
        fs::write(tmp.path().join("c"), "").unwrap();
        fs::write(tmp.path().join("config.yaml"), "").unwrap();

        let result = probe_filesystem("cat", "c", tmp.path());
        assert_eq!(result, Some("c".to_string())); // Shorter name wins
    }

    #[test]
    fn test_probe_any_preference() {
        let tmp = TempDir::new().unwrap();
        fs::create_dir(tmp.path().join("dir")).unwrap();
        fs::write(tmp.path().join("data"), "").unwrap();

        // For "cp" (Any preference), both are equal priority, shorter wins
        let result = probe_filesystem("cp", "d", tmp.path());
        assert!(result == Some("dir/".to_string()) || result == Some("data".to_string()));
    }

    #[test]
    fn test_resolve_scan_target_simple() {
        let cwd = PathBuf::from("/home/user");
        let (dir, prefix) = resolve_scan_target("test", &cwd);
        assert_eq!(dir, PathBuf::from("/home/user"));
        assert_eq!(prefix, "test");
    }

    #[test]
    fn test_resolve_scan_target_with_slash() {
        let cwd = PathBuf::from("/home/user");
        let (dir, prefix) = resolve_scan_target("dir/test", &cwd);
        assert_eq!(dir, PathBuf::from("/home/user/dir/"));
        assert_eq!(prefix, "test");
    }

    #[test]
    fn test_reconstruct_path_file() {
        let result = reconstruct_path("co", "co", "cool", false);
        assert_eq!(result, "cool");
    }

    #[test]
    fn test_reconstruct_path_dir() {
        let result = reconstruct_path("co", "co", "configs", true);
        assert_eq!(result, "configs/");
    }

    #[test]
    fn test_reconstruct_path_with_base() {
        let result = reconstruct_path("dir/co", "co", "cool", false);
        assert_eq!(result, "dir/cool");
    }
}
