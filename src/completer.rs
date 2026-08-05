/// Tab completion engine: context-aware completion for commands, paths, variables,
/// with configurable completion specs (Phase 7).
use crate::environment::ShellState;
use std::collections::HashMap;
use std::fs;
use std::path::{Path, PathBuf};

const MAX_COMPLETION_ITEMS: usize = 4096;
const MAX_COMPLETION_TEXT_BYTES: usize = 16 * 1024;
const MAX_COMPLETION_PROJECT_FILE_BYTES: usize = 4 * 1024 * 1024;
const MAX_GIT_COMPLETION_BYTES: usize = 8 * 1024 * 1024;

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum CompletionKind {
    Command,
    Builtin,
    Alias,
    Function,
    Directory,
    File,
    Variable,
    Subcommand,
    Flag,
    Other,
}

#[derive(Debug, Clone)]
pub struct Completion {
    pub text: String,
    pub display: String,
    pub description: Option<String>,
    pub kind: CompletionKind,
    pub is_dir: bool,
}

impl Completion {
    fn new(text: String, kind: CompletionKind) -> Self {
        let is_dir = kind == CompletionKind::Directory;
        Completion {
            display: text.clone(),
            text,
            description: None,
            kind,
            is_dir,
        }
    }

    fn with_desc(mut self, desc: &str) -> Self {
        self.description = Some(desc.to_string());
        self
    }
}

/// Completion cache entry with frequency tracking
#[derive(Debug, Clone)]
struct CacheEntry {
    completions: Vec<Completion>,
    hit_count: u32,
}

/// LRU completion cache
#[derive(Debug)]
struct CompletionCache {
    cache: HashMap<String, CacheEntry>,
    max_size: usize,
}

impl CompletionCache {
    fn new(max_size: usize) -> Self {
        CompletionCache {
            cache: HashMap::new(),
            max_size,
        }
    }

    fn get(&mut self, key: &str) -> Option<Vec<Completion>> {
        if let Some(entry) = self.cache.get_mut(key) {
            entry.hit_count += 1;
            return Some(entry.completions.clone());
        }
        None
    }

    fn insert(&mut self, key: String, completions: Vec<Completion>) {
        if self.cache.len() >= self.max_size && !self.cache.contains_key(&key) {
            // Remove the least frequently used entry
            if let Some(lfu_key) = self
                .cache
                .iter()
                .min_by_key(|(_, entry)| entry.hit_count)
                .map(|(k, _)| k.clone())
            {
                self.cache.remove(&lfu_key);
            }
        }

        self.cache.insert(
            key,
            CacheEntry {
                completions,
                hit_count: 0,
            },
        );
    }

    fn clear(&mut self) {
        self.cache.clear();
    }
}

// Thread-local cache for completion results
thread_local! {
    static COMPLETION_CACHE: std::cell::RefCell<CompletionCache> =
        std::cell::RefCell::new(CompletionCache::new(256));
    /// Raw output of each external probe and each project file read, for the
    /// current command line. Keyed by source rather than by prefix: the
    /// completion cache above keys on the typed word, so without this a
    /// growing prefix re-forks Git or Docker on every keystroke.
    static PROBE_CACHE: std::cell::RefCell<HashMap<String, Option<Vec<u8>>>> =
        std::cell::RefCell::new(HashMap::new());
}

const MAX_PROBE_CACHE_ENTRIES: usize = 64;

/// Run a probe at most once per source per command line. A probe that fails
/// or finds nothing is remembered too — a stopped Docker daemon must cost one
/// timeout while a word is typed, not one per keystroke.
fn probe_once(key: &str, produce: impl FnOnce() -> Option<Vec<u8>>) -> Option<Vec<u8>> {
    if let Some(cached) = PROBE_CACHE.with(|cache| cache.borrow().get(key).cloned()) {
        return cached;
    }
    let produced = produce();
    PROBE_CACHE.with(|cache| {
        let mut cache = cache.borrow_mut();
        if cache.len() < MAX_PROBE_CACHE_ENTRIES {
            cache.insert(key.to_string(), produced.clone());
        }
    });
    produced
}

/// [`probe_once`] for the probes whose output is text.
fn probe_text_once(key: &str, produce: impl FnOnce() -> Option<String>) -> Option<String> {
    probe_once(key, || produce().map(String::into_bytes))
        .map(|bytes| String::from_utf8_lossy(&bytes).into_owned())
}

pub fn complete(buffer: &str, cursor: usize, state: &mut ShellState) -> (usize, Vec<Completion>) {
    let buf = &buffer[..cursor];
    let (word, word_start) = extract_word_at(buf);
    let is_cmd_pos = is_command_position(buf, word_start);

    // Everything after the command name reasons about the alias-expanded
    // line: with `alias gs='git status'`, `gs -<TAB>` is a `git status` flag
    // position, not the first argument of some command named `gs`.
    let segment_start = active_command_segment_start(&buf[..word_start]);
    let typed_before = &buf[segment_start..word_start];
    let expanded_before = alias_expanded_segment(typed_before, &state.aliases);
    let cmd = command_words(expanded_before.trim())
        .next()
        .unwrap_or("")
        .to_string();

    // `sudo -u <TAB>` sits at what the wrapper-skipping logic counts as a
    // command position; the value the option expects is a user or group.
    let wrapper_value = wrapper_value_kind(&buf[..word_start]);

    // A redirection target is a plain file wherever it appears. Without this
    // the word still belongs to the command being written, so `git add > n`
    // would offer dirty files and `cd > n` only directories.
    let redirect_target = is_redirect_target(&buf[..word_start]);

    // Create cache key based on context
    let cache_key = if redirect_target {
        format!("redir:{word}")
    } else if let Some(kind) = wrapper_value {
        format!("wrapval:{kind:?}:{word}")
    } else if is_cmd_pos {
        format!("cmd:{}", word)
    } else if word.starts_with('$') && !word.contains('/') {
        format!("var:{word}")
    } else {
        // Argument completion depends on the full command and repository
        // context, not just the last word (which is often empty after a space).
        format!(
            "arg:{}:{}:{}:{}:{}",
            cmd,
            &buf[..word_start],
            word,
            state.cached_git_branch.as_deref().unwrap_or(""),
            state.cached_git_remote.as_deref().unwrap_or("")
        )
    };

    // Try to get from cache
    let cached = COMPLETION_CACHE.with(|cache| cache.borrow_mut().get(&cache_key));

    if let Some(completions) = cached {
        return (word_start, completions);
    }

    // Check user-defined completion specs first
    if !is_cmd_pos {
        if let Some(spec) = state.completion_specs.get(&cmd).cloned() {
            let completions = finalize_completions(apply_completion_spec(&spec, &word, state));
            if !completions.is_empty() {
                COMPLETION_CACHE.with(|cache| {
                    cache.borrow_mut().insert(cache_key, completions.clone());
                });
                return (word_start, completions);
            }
        }
    }

    // Detect if we're after a pipe for smart recommendations
    let after_pipe = {
        let before = buf[..word_start].trim_end();
        before.ends_with('|') && !before.ends_with("||")
    };

    // A `/` ends the variable name: `$HOME` is still being typed, while
    // `$HOME/pro` is a path whose first segment happens to be a variable.
    let variable_word = word.starts_with('$') && !word.contains('/');

    let completions = if let Some(variable) = word.strip_prefix("${").filter(|_| variable_word) {
        complete_variable_braced(variable, state)
    } else if let Some(variable) = word.strip_prefix('$').filter(|_| variable_word) {
        complete_variable(variable, state)
    } else if redirect_target {
        complete_path(&word, state)
    } else if let Some(kind) = wrapper_value {
        match kind {
            WrapperValueKind::User => complete_users(&word),
            WrapperValueKind::Group => complete_groups(&word),
        }
    } else if let Some(offset) = assignment_value_start(&word) {
        // `VAR=/pa<TAB>` and `export VAR=/pa<TAB>`: complete the value as a
        // path and keep the assignment prefix on the inserted text.
        complete_path(&word[offset..], state)
            .into_iter()
            .map(|mut completion| {
                completion.text = format!("{}{}", &word[..offset], completion.text);
                completion
            })
            .collect()
    } else if is_cmd_pos && after_pipe {
        // Smart pipe completion: recommend based on preceding command
        let mut pipe_completions = complete_pipe_targets(buf, &word);
        if pipe_completions.is_empty() {
            complete_command(&word, state)
        } else {
            // Also include regular command completions after pipe suggestions
            let mut regular = complete_command(&word, state);
            pipe_completions.append(&mut regular);
            pipe_completions
        }
    } else if is_cmd_pos {
        let mut cmd_completions = complete_command(&word, state);
        // Append project-aware completions for short prefixes
        if word.len() <= 3 {
            let project = complete_project_commands(&word);
            cmd_completions.extend(project);
        }
        cmd_completions
    } else if let Some(fields) = pipeline_field_completions(&cmd, &word, buf, word_start, state) {
        fields
    } else if let Some(subs) = subcommand_completions(&cmd, &word, &expanded_before, state) {
        subs
    } else if let Some(spec_completions) = complete_from_spec(&cmd, &word, &expanded_before, state)
    {
        spec_completions
    } else if cmd == "cd" {
        // Local directories; when none match, fall back to the frecency
        // database so `cd proj<TAB>` works from anywhere.
        let results: Vec<Completion> = complete_path(&word, state)
            .into_iter()
            .filter(|c| c.is_dir)
            .collect();
        if results.is_empty() {
            z_frecency_completions(&word)
        } else {
            results
        }
    } else if cmd == "mkdir" || cmd == "rmdir" {
        complete_path(&word, state)
            .into_iter()
            .filter(|c| c.is_dir)
            .collect()
    } else if cmd == "z" {
        // Frequently used directories first, then plain subdirectories.
        let mut results = z_frecency_completions(&word);
        results.extend(complete_path(&word, state).into_iter().filter(|c| c.is_dir));
        results
    } else {
        // Paths; when none match, fall back to arguments this command has
        // been given before.
        let results = filter_by_file_type(complete_path(&word, state), &cmd);
        if results.is_empty() {
            complete_history_arguments(&cmd, &word, state)
        } else {
            results
        }
    };

    let completions = requote_completions(completions, &word);
    // What was accepted here before leads, among candidates the typed text
    // matches equally well. Never for the command name itself: there the
    // prefix decides and a learned order would fight it.
    let completions = if is_cmd_pos {
        completions
    } else {
        promote_accepted(completions, &cmd)
    };
    let completions = finalize_completions(completions);

    // Store in cache
    COMPLETION_CACHE.with(|cache| {
        cache.borrow_mut().insert(cache_key, completions.clone());
    });

    (word_start, completions)
}

/// Central terminal/execution boundary for every completion source. Candidate
/// text is later inserted into the editable command line, so values containing
/// controls or invisible Unicode are dropped rather than displayed as an
/// escaped spelling and executed as something else. Display-only metadata is
/// escaped and byte-bounded.
fn finalize_completions(completions: Vec<Completion>) -> Vec<Completion> {
    let mut seen = std::collections::HashSet::new();
    completions
        .into_iter()
        .filter(|completion| {
            completion.text.len() <= MAX_COMPLETION_TEXT_BYTES
                && crate::terminal_text::is_safe_inline(&completion.text)
                // Merged sources repeat texts (pipe suggestions and PATH
                // commands, refs and files); the first, higher-priority
                // spelling wins.
                && seen.insert(completion.text.clone())
        })
        .take(MAX_COMPLETION_ITEMS)
        .map(|mut completion| {
            completion.display =
                crate::terminal_text::escape_inline(&completion.display, MAX_COMPLETION_TEXT_BYTES);
            completion.description = completion.description.as_deref().map(|description| {
                crate::terminal_text::escape_inline(description, MAX_COMPLETION_TEXT_BYTES)
            });
            completion
        })
        .collect()
}

fn apply_completion_spec(
    spec: &crate::environment::CompletionSpec,
    prefix: &str,
    state: &mut ShellState,
) -> Vec<Completion> {
    let mut completions = Vec::new();

    // -W word list
    if let Some(ref words) = spec.word_list {
        for w in words {
            if w.starts_with(prefix) {
                completions.push(Completion {
                    text: w.clone(),
                    display: w.clone(),
                    description: None,
                    kind: CompletionKind::Other,
                    is_dir: false,
                });
            }
        }
    }

    // -F function
    if let Some(ref func_name) = spec.function {
        if let Some(func_body) = state.functions.get(func_name).cloned() {
            // Set completion variables - push a local scope for these variables
            state.push_local_scope();
            let line = prefix; // simplified
            let words: Vec<String> = line.split_whitespace().map(|s| s.to_string()).collect();
            state.arrays.insert("COMP_WORDS".to_string(), words.clone());
            if let Some(scope) = state.local_vars_stack.last_mut() {
                scope.insert(
                    "COMP_CWORD".to_string(),
                    (words.len().saturating_sub(1)).to_string(),
                );
                scope.insert("COMP_LINE".to_string(), line.to_string());
                scope.insert("COMP_POINT".to_string(), line.len().to_string());
            }
            state.arrays.insert("COMPREPLY".to_string(), Vec::new());

            // Execute the function
            crate::executor::execute_compound(&func_body, state);

            // Read COMPREPLY
            if let Some(replies) = state.arrays.get("COMPREPLY") {
                for reply in replies {
                    if reply.starts_with(prefix) {
                        completions.push(Completion {
                            text: reply.clone(),
                            display: reply.clone(),
                            description: None,
                            kind: CompletionKind::Other,
                            is_dir: false,
                        });
                    }
                }
            }

            // Clean up - pop the local scope
            state.pop_local_scope();
            state.arrays.remove("COMP_WORDS");
            state.arrays.remove("COMPREPLY");
        }
    }

    // -d directory
    if spec.directory {
        completions.extend(
            complete_path(prefix, state)
                .into_iter()
                .filter(|c| c.is_dir),
        );
    }

    // -f file
    if spec.file {
        completions.extend(complete_path(prefix, state));
    }

    // -X filter pattern
    if let Some(ref pattern) = spec.filter_pattern {
        completions.retain(|c| !crate::glob_match::glob_match(pattern, &c.text));
    }

    // -P prefix, -S suffix
    if let Some(ref pfx) = spec.prefix {
        for c in &mut completions {
            c.text = format!("{}{}", pfx, c.text);
        }
    }
    if let Some(ref sfx) = spec.suffix {
        for c in &mut completions {
            c.text = format!("{}{}", c.text, sfx);
        }
    }

    completions
}

/// The command an alias stands for, following one-word aliases and taking
/// the head word of a multi-word one.
pub fn resolve_alias(cmd: &str, aliases: &HashMap<String, String>) -> String {
    let expanded = alias_expanded_segment(cmd, aliases);
    expanded
        .split_whitespace()
        .next()
        .unwrap_or(cmd)
        .to_string()
}

/// A one-word alias is a transparent command wrapper for completion (`g=git`).
fn resolve_transparent_alias(mut cmd: String, aliases: &HashMap<String, String>) -> String {
    for _ in 0..8 {
        let Some(expansion) = aliases.get(&cmd) else {
            break;
        };
        let mut words = expansion.split_whitespace();
        let Some(target) = words.next() else {
            break;
        };
        if words.next().is_some() || target == cmd {
            break;
        }
        cmd = target.to_string();
    }
    cmd
}

/// The active command with its leading alias expanded, as the words a
/// completion should reason about.
///
/// `alias gs='git status'` makes `gs -<TAB>` a `git status` flag position,
/// not the first argument of a command called `gs`. Only the head word is
/// substituted, and only while it keeps naming an alias, so the expansion of
/// a multi-word alias contributes its own subcommands and options exactly as
/// if they had been typed.
fn alias_expanded_segment(segment: &str, aliases: &HashMap<String, String>) -> String {
    let mut expanded = segment.to_string();
    for _ in 0..8 {
        let words: Vec<&str> = expanded.split_whitespace().collect();
        let index = effective_command_index(&words);
        let Some(head) = words.get(index).copied() else {
            break;
        };
        let Some(expansion) = aliases.get(head) else {
            break;
        };
        // `alias ls='ls --color=auto'` is the idiomatic self-reference: it
        // expands once and then stands for the real command, exactly as the
        // shell itself resolves it.
        let self_referential = expansion.split_whitespace().next() == Some(head);
        let Some(offset) = word_offset(&expanded, index) else {
            break;
        };
        expanded.replace_range(offset..offset + head.len(), expansion);
        if self_referential {
            break;
        }
    }
    expanded
}

/// Byte offset of the nth whitespace-separated word.
fn word_offset(text: &str, index: usize) -> Option<usize> {
    text.split_whitespace()
        .nth(index)
        .map(|word| word.as_ptr() as usize - text.as_ptr() as usize)
}

fn first_command(buf: &str) -> String {
    command_words(active_command_segment(buf))
        .next()
        .unwrap_or("")
        .to_string()
}

fn command_words(segment: &str) -> impl Iterator<Item = &str> {
    let words: Vec<&str> = segment.split_whitespace().collect();
    let command_index = effective_command_index(&words);
    words.into_iter().skip(command_index)
}

fn effective_command_index(words: &[&str]) -> usize {
    let mut index = 0;
    loop {
        while index < words.len() && is_assignment_word(words[index]) {
            index += 1;
        }
        let Some(wrapper) = words.get(index).copied() else {
            return index;
        };
        match wrapper {
            "sudo" => {
                index += 1;
                index = skip_wrapper_options(
                    words,
                    index,
                    &[
                        "-u",
                        "--user",
                        "-g",
                        "--group",
                        "-h",
                        "--host",
                        "-p",
                        "--prompt",
                        "-C",
                        "--close-from",
                        "-T",
                        "--command-timeout",
                        "-R",
                        "--chroot",
                        "-D",
                        "--chdir",
                    ],
                );
            }
            "env" => {
                index += 1;
                index = skip_wrapper_options(
                    words,
                    index,
                    &["-u", "--unset", "-C", "--chdir", "-S", "--split-string"],
                );
            }
            "command" | "builtin" | "nohup" => {
                index += 1;
                index = skip_wrapper_options(words, index, &[]);
            }
            "exec" | "time" => {
                index += 1;
                index = skip_wrapper_options(words, index, &["-a", "-f", "-o"]);
            }
            "nice" => {
                index += 1;
                index = skip_wrapper_options(words, index, &["-n", "--adjustment"]);
            }
            // Keywords a command follows. `while read x; do gr<TAB>` is a
            // command position, not an argument of `do`.
            "do" | "then" | "else" | "elif" | "if" | "while" | "until" | "!" | "{" => {
                index += 1;
            }
            _ => return index,
        }
    }
}

fn skip_wrapper_options(words: &[&str], mut index: usize, value_options: &[&str]) -> usize {
    while let Some(word) = words.get(index).copied() {
        if word == "--" {
            return index + 1;
        }
        if is_assignment_word(word) {
            index += 1;
            continue;
        }
        if !word.starts_with('-') || word == "-" {
            break;
        }
        let option = word.split_once('=').map(|(name, _)| name).unwrap_or(word);
        index += 1;
        if !word.contains('=') && value_options.contains(&option) {
            index = (index + 1).min(words.len());
        }
    }
    index
}

/// Resolve the leading `~`, `~user` or `$VAR` of a path being completed to
/// the directory that must actually be scanned. The candidate text keeps the
/// spelling the user typed — a `$HOME/` stays `$HOME/` in the command line —
/// so only the lookup is expanded here.
fn expand_completion_prefix(lookup_prefix: &str, state: &ShellState) -> Option<String> {
    if let Some(rest) = lookup_prefix.strip_prefix('~') {
        let home = state.home_dir.to_string_lossy();
        if rest.is_empty() {
            return Some(format!("{home}/"));
        }
        if let Some(rest) = rest.strip_prefix('/') {
            return Some(format!("{home}/{rest}"));
        }
        // `~user/...`: the home directory comes from the user database.
        let (user, rest) = rest.split_once('/')?;
        let home = passwd_home_dir(user)?;
        return Some(format!("{}/{}", home.trim_end_matches('/'), rest));
    }

    // `$VAR/...` and `${VAR}/...`. Only a whole leading segment is resolved:
    // `$HOME` is a directory to scan, while `pre$VAR` is a pattern this
    // completion has no way to invert.
    if let Some(rest) = lookup_prefix.strip_prefix('$') {
        let (name, rest) = match rest.strip_prefix('{') {
            Some(braced) => {
                let (name, rest) = braced.split_once('}')?;
                (name, rest)
            }
            None => {
                let end = rest
                    .find(|ch: char| !ch.is_alphanumeric() && ch != '_')
                    .unwrap_or(rest.len());
                rest.split_at(end)
            }
        };
        if !rest.starts_with('/') {
            return None;
        }
        let value = state.get_var(name)?;
        return Some(format!("{}{}", value.trim_end_matches('/'), rest));
    }

    Some(lookup_prefix.to_string())
}

/// The home directory recorded for one user, or `None` when the user is not
/// in the database or the entry has no home field.
fn passwd_home_dir(user: &str) -> Option<String> {
    let content =
        crate::io_guard::read_regular_text(Path::new("/etc/passwd"), MAX_USER_DB_BYTES).ok()?;
    content.lines().find_map(|line| {
        let mut fields = line.split(':');
        (fields.next()? == user).then(|| fields.nth(4).map(str::to_string))?
    })
}

/// `~alice<TAB>` — user names spelled as the home directory they stand for,
/// so accepting one continues into that directory.
fn complete_user_homes(prefix: &str) -> Vec<Completion> {
    rank_prefix_then_fuzzy(complete_users(""), prefix)
        .into_iter()
        .map(|mut completion| {
            completion.display = format!("~{}", completion.text);
            completion.text = format!("~{}/", completion.text);
            completion.is_dir = true;
            completion.kind = CompletionKind::Directory;
            completion
        })
        .collect()
}

/// Extensions a command is normally pointed at. Directories always stay —
/// they are the way to reach the file — and a command not listed here keeps
/// every candidate.
fn command_file_extensions(cmd: &str) -> Option<&'static [&'static str]> {
    Some(match cmd {
        "source" | "." | "bash" | "sh" | "zsh" | "jsh" => &["sh", "bash", "zsh", "jsh", "rc"],
        "python" | "python3" => &["py", "pyc", "pyz"],
        "node" | "nodejs" => &["js", "mjs", "cjs", "ts"],
        "ruby" => &["rb"],
        "perl" => &["pl", "pm"],
        "unzip" | "zipinfo" => &["zip", "jar", "war", "aar", "whl", "egg", "apk"],
        "gunzip" | "zcat" => &["gz", "tgz", "z"],
        "bunzip2" | "bzcat" => &["bz2", "tbz", "tbz2"],
        "unxz" | "xzcat" => &["xz", "txz"],
        "gzip" | "bzip2" | "xz" | "zstd" => return None,
        "rustc" => &["rs"],
        "javac" => &["java"],
        "java" => &["jar", "class"],
        "go" => &["go"],
        "docker-compose" => &["yml", "yaml"],
        "psql" | "sqlite3" => &["sql", "db", "sqlite", "sqlite3"],
        _ => return None,
    })
}

/// Keep only the candidates a command can actually open, when it is a command
/// whose argument is a specific kind of file. A prefix that already matches
/// nothing of that kind keeps the unfiltered list rather than showing an
/// empty menu: the guess is a convenience, never a restriction.
fn filter_by_file_type(completions: Vec<Completion>, cmd: &str) -> Vec<Completion> {
    let Some(extensions) = command_file_extensions(cmd) else {
        return completions;
    };
    let filtered: Vec<Completion> = completions
        .iter()
        .filter(|completion| {
            completion.is_dir
                || Path::new(completion.display.as_str())
                    .extension()
                    .and_then(|extension| extension.to_str())
                    .is_some_and(|extension| {
                        extensions
                            .iter()
                            .any(|candidate| candidate.eq_ignore_ascii_case(extension))
                    })
        })
        .cloned()
        .collect();
    // Only directories left means the filter found nothing of its own kind.
    if filtered.iter().any(|completion| !completion.is_dir) {
        filtered
    } else {
        completions
    }
}

/// Is the word being completed the target of a redirection? True for the
/// operators that name a file (`>`, `>>`, `<`, `2>`, `&>`, `<>`), false for
/// `>&`/`<&`, whose operand is a file descriptor.
fn is_redirect_target(before: &str) -> bool {
    let before = before.trim_end_matches([' ', '\t']);
    if before.ends_with('&') {
        return false;
    }
    let mut quote = None;
    let mut escaped = false;
    let mut last_operator = false;
    for ch in before.chars() {
        if escaped {
            escaped = false;
            last_operator = false;
            continue;
        }
        match quote {
            Some('\'') => {
                if ch == '\'' {
                    quote = None;
                }
            }
            Some('"') => match ch {
                '"' => quote = None,
                '\\' => escaped = true,
                _ => {}
            },
            _ => match ch {
                '\\' => escaped = true,
                '\'' | '"' => {
                    quote = Some(ch);
                    last_operator = false;
                }
                '<' | '>' => last_operator = true,
                ch if ch.is_whitespace() => {}
                _ => last_operator = false,
            },
        }
    }
    last_operator
}

/// An unclosed quote opening the word being completed, with the text inside
/// it. `cat "my fi` completes inside the quotes rather than replacing them
/// with backslash escapes.
fn open_quote_context(word: &str) -> Option<(char, &str)> {
    let quote = word.chars().next().filter(|ch| *ch == '\'' || *ch == '"')?;
    let inner = &word[quote.len_utf8()..];
    // A quote the user already closed is not an open context; the word after
    // it is ordinary text again.
    let closed = match quote {
        '\'' => inner.contains('\''),
        _ => {
            let mut escaped = false;
            inner.chars().any(|ch| {
                let closes = !escaped && ch == '"';
                escaped = !escaped && ch == '\\';
                closes
            })
        }
    };
    (!closed).then_some((quote, inner))
}

/// Re-spell path candidates in the quoting style the word already uses.
/// A file closes the quote, so the next keystroke lands outside it; a
/// directory leaves it open, because the path continues.
fn requote_completions(completions: Vec<Completion>, word: &str) -> Vec<Completion> {
    let Some((quote, _)) = open_quote_context(word) else {
        return completions;
    };
    completions
        .into_iter()
        .map(|mut completion| {
            let raw = unescape_shell_word(&completion.text);
            // A quote cannot appear inside itself; leave those escaped.
            if !raw.contains(quote) {
                completion.text = if completion.is_dir {
                    format!("{quote}{raw}")
                } else {
                    format!("{quote}{raw}{quote}")
                };
            }
            completion
        })
        .collect()
}

/// Byte offset of the value in a `NAME=value` or `NAME+=value` word, if the
/// word is a well-formed shell assignment.
fn assignment_value_start(word: &str) -> Option<usize> {
    let eq = word.find('=')?;
    let name = word[..eq].strip_suffix('+').unwrap_or(&word[..eq]);
    let mut chars = name.chars();
    let valid = matches!(chars.next(), Some(ch) if ch == '_' || ch.is_ascii_alphabetic())
        && chars.all(|ch| ch == '_' || ch.is_ascii_alphanumeric());
    valid.then_some(eq + 1)
}

fn is_assignment_word(word: &str) -> bool {
    let Some((name, _)) = word.split_once('=') else {
        return false;
    };
    let mut chars = name.chars();
    matches!(chars.next(), Some(ch) if ch == '_' || ch.is_ascii_alphabetic())
        && chars.all(|ch| ch == '_' || ch.is_ascii_alphanumeric())
}

pub(crate) fn active_command_segment(buf: &str) -> &str {
    let start = active_command_segment_start(buf);
    buf[start..].trim_start()
}

fn active_command_segment_start(buf: &str) -> usize {
    // Each parenthesis level tracks its own most recent command separator, so
    // separators inside `$(...)` do not leak into the outer command after `)`.
    let mut starts = vec![0usize];
    let mut quote = None;
    let mut escaped = false;
    let chars: Vec<(usize, char)> = buf.char_indices().collect();
    for (position, (index, ch)) in chars.iter().copied().enumerate() {
        if escaped {
            escaped = false;
            continue;
        }
        match quote {
            Some('\'') => {
                if ch == '\'' {
                    quote = None;
                }
            }
            Some('"') => match ch {
                '"' => quote = None,
                '\\' => escaped = true,
                _ => {}
            },
            _ => match ch {
                '\\' => escaped = true,
                '\'' | '"' => quote = Some(ch),
                '(' => starts.push(index + ch.len_utf8()),
                ')' if starts.len() > 1 => {
                    starts.pop();
                }
                // A backtick opens a command substitution and the next one
                // closes it, so each is the start of a fresh command.
                '`' => {
                    *starts.last_mut().unwrap() = index + ch.len_utf8();
                }
                ';' | '\n' | '|' => {
                    *starts.last_mut().unwrap() = index + ch.len_utf8();
                }
                '&' => {
                    let previous = position.checked_sub(1).map(|p| chars[p].1);
                    let next = chars.get(position + 1).map(|(_, ch)| *ch);
                    // `&>` and `>&` are redirections, not command separators.
                    if previous != Some('>') && next != Some('>') {
                        *starts.last_mut().unwrap() = index + ch.len_utf8();
                    }
                }
                _ => {}
            },
        }
    }
    *starts.last().unwrap_or(&0)
}

fn subcommand_completions(
    cmd: &str,
    prefix: &str,
    before: &str,
    state: &mut ShellState,
) -> Option<Vec<Completion>> {
    let before = before.trim();
    let words: Vec<&str> = command_words(before).collect();
    let word_count = words.len();

    if cmd == "cargo" {
        if let Some((option, value)) = prefix.split_once('=') {
            let kind = match option {
                "--bin" => Some(CargoArgKind::Bin),
                "--example" => Some(CargoArgKind::Example),
                "--package" => Some(CargoArgKind::Package),
                "--features" => Some(CargoArgKind::Feature),
                _ => None,
            };
            if let Some(kind) = kind {
                let results = rank_prefix_then_fuzzy(complete_cargo_argument("", kind), value)
                    .into_iter()
                    .map(|mut completion| {
                        completion.text = format!("{}={}", option, completion.text);
                        completion
                    })
                    .collect::<Vec<_>>();
                if !results.is_empty() {
                    return Some(results);
                }
            }
        }
    }

    // Project-native dynamic arguments. Keep flags delegated to JSON specs.
    if !prefix.starts_with('-') {
        let node_run = matches!(cmd, "npm" | "pnpm" | "bun")
            && words.get(1) == Some(&"run")
            && word_count == 2;
        let yarn_run =
            cmd == "yarn" && (word_count == 1 || (words.get(1) == Some(&"run") && word_count == 2));
        if node_run || yarn_run {
            let results = rank_prefix_then_fuzzy(complete_npm_scripts(""), prefix);
            if !results.is_empty() {
                return Some(results);
            }
        }
        let node_uninstall = matches!(
            (cmd, words.get(1).copied().unwrap_or("")),
            (
                "npm",
                "uninstall" | "remove" | "rm" | "un" | "update" | "up"
            ) | (
                "pnpm" | "bun",
                "remove" | "rm" | "uninstall" | "update" | "up"
            ) | ("yarn", "remove" | "upgrade")
        );
        if node_uninstall && word_count >= 2 {
            let results = rank_prefix_then_fuzzy(complete_npm_dependencies(""), prefix);
            if !results.is_empty() {
                return Some(results);
            }
        }
        if cmd == "make" && word_count == 1 {
            let results = rank_prefix_then_fuzzy(complete_make_targets(""), prefix);
            if !results.is_empty() {
                return Some(results);
            }
        }
        if cmd == "cargo" && word_count >= 3 {
            let kind = match words.last().copied() {
                Some("--bin") => Some(CargoArgKind::Bin),
                Some("--example") => Some(CargoArgKind::Example),
                Some("--package" | "-p") => Some(CargoArgKind::Package),
                Some("--features" | "-F") => Some(CargoArgKind::Feature),
                _ => None,
            };
            if let Some(kind) = kind {
                let results = rank_prefix_then_fuzzy(complete_cargo_argument("", kind), prefix);
                if !results.is_empty() {
                    return Some(results);
                }
            }
        }
    }

    // Remote destinations for the ssh family, from ~/.ssh/config and
    // known_hosts. An option that expects a local value (`ssh -i <key>`)
    // keeps the default path completion.
    if matches!(
        cmd,
        "ssh" | "sftp" | "mosh" | "ssh-copy-id" | "scp" | "rsync"
    ) && !prefix.starts_with('-')
    {
        let prev = words.last().copied().unwrap_or("");
        let prev_takes_value = SSH_VALUE_OPTIONS.contains(&prev);
        let copy_command = matches!(cmd, "scp" | "rsync");
        // For ssh itself only the destination is a host; anything after it is
        // a remote command and keeps path completion.
        let destination_open = copy_command || ssh_positional_count(&words) == 0;
        if !prev_takes_value && destination_open {
            let mut results = complete_ssh_hosts(prefix, copy_command, &state.home_dir);
            if copy_command {
                results.extend(complete_path(prefix, state));
            }
            if !results.is_empty() {
                return Some(results);
            }
        }
    }

    // Shell-state arguments: variable names for the declaration builtins,
    // alias names, command names, and job specs. These are never paths, so a
    // miss returns an empty list rather than falling back to files.
    if matches!(
        cmd,
        "export" | "unset" | "readonly" | "declare" | "typeset" | "local"
    ) && !prefix.starts_with('-')
        && !prefix.contains('=')
    {
        if cmd == "unset" && words.iter().skip(1).any(|w| *w == "-f") {
            let mut names: Vec<&String> = state.functions.keys().collect();
            names.sort();
            let completions = names
                .into_iter()
                .map(|n| Completion::new(n.clone(), CompletionKind::Function).with_desc("function"))
                .collect();
            return Some(rank_prefix_then_fuzzy(completions, prefix));
        }
        return Some(rank_prefix_then_fuzzy(
            complete_variable_names("", state),
            prefix,
        ));
    }

    if matches!(cmd, "alias" | "unalias") && !prefix.starts_with('-') && !prefix.contains('=') {
        let mut entries: Vec<(&String, &String)> = state.aliases.iter().collect();
        entries.sort_by_key(|(name, _)| name.as_str());
        let completions = entries
            .into_iter()
            .map(|(name, expansion)| {
                Completion::new(name.clone(), CompletionKind::Alias)
                    .with_desc(&format!("alias for {expansion}"))
            })
            .collect();
        return Some(rank_prefix_then_fuzzy(completions, prefix));
    }

    // `man 3 printf` names a section first. Digits would otherwise fuzzy
    // match every command with a digit in it, which is not what was asked.
    let man_section =
        cmd == "man" && !prefix.is_empty() && prefix.chars().all(|ch| ch.is_ascii_digit());
    if matches!(cmd, "which" | "type" | "whereis" | "man")
        && !prefix.starts_with('-')
        && !man_section
    {
        let results = complete_command(prefix, state);
        if !results.is_empty() {
            return Some(results);
        }
    }

    if matches!(cmd, "fg" | "bg" | "disown" | "wait") || (cmd == "kill" && !prefix.starts_with('-'))
    {
        let mut results = complete_job_specs(prefix, state);
        if matches!(cmd, "kill" | "wait") {
            for job in &state.jobs.jobs {
                let pid = job.pid.as_raw().to_string();
                if pid.starts_with(prefix) {
                    results.push(
                        Completion::new(pid, CompletionKind::Other)
                            .with_desc(&format!("{} — {}", job.status, job.command)),
                    );
                }
            }
        }
        if cmd == "kill" {
            let known: std::collections::HashSet<String> =
                results.iter().map(|c| c.text.clone()).collect();
            results.extend(
                complete_system_pids(prefix)
                    .into_iter()
                    .filter(|c| !known.contains(&c.text)),
            );
        }
        return Some(results);
    }

    if cmd == "kill" && prefix.starts_with('-') {
        let results: Vec<Completion> = KILL_SIGNALS
            .iter()
            .filter(|(sig, _)| sig.starts_with(prefix))
            .map(|(sig, desc)| {
                Completion::new(sig.to_string(), CompletionKind::Flag).with_desc(desc)
            })
            .collect();
        if !results.is_empty() {
            return Some(results);
        }
    }

    // Test operators. `[ -f x ]` and `[[ -z "$v" ]]` are where the single
    // letters are hardest to remember, and they are a closed set.
    if matches!(cmd, "test" | "[" | "[[") && prefix.starts_with('-') {
        let completions = TEST_OPERATORS
            .iter()
            .map(|(operator, desc)| {
                Completion::new((*operator).to_string(), CompletionKind::Flag).with_desc(desc)
            })
            .collect();
        let ranked = rank_prefix_then_fuzzy(completions, prefix);
        if !ranked.is_empty() {
            return Some(ranked);
        }
    }

    // `man 3 printf`: a section number narrows the page, and each section
    // holds a different kind of thing.
    if cmd == "man" && word_count == 1 && prefix.chars().all(|ch| ch.is_ascii_digit()) {
        let sections = [
            ("1", "user commands"),
            ("2", "system calls"),
            ("3", "library functions"),
            ("4", "device files"),
            ("5", "file formats"),
            ("6", "games"),
            ("7", "conventions and miscellany"),
            ("8", "system administration"),
        ];
        let completions = sections
            .iter()
            .map(|(section, desc)| {
                Completion::new((*section).to_string(), CompletionKind::Other).with_desc(desc)
            })
            .collect();
        let ranked = rank_prefix_then_fuzzy(completions, prefix);
        if !ranked.is_empty() {
            return Some(ranked);
        }
    }

    // Symbolic and numeric modes for the permission commands.
    if matches!(cmd, "chmod") && word_count == 1 && !prefix.starts_with('-') {
        let modes: &[(&str, &str)] = &[
            ("644", "rw-r--r-- — a file others may read"),
            ("600", "rw------- — a private file"),
            ("755", "rwxr-xr-x — a program or directory"),
            ("700", "rwx------ — a private directory"),
            ("664", "rw-rw-r-- — group writable"),
            ("777", "rwxrwxrwx — everyone, everything"),
            ("u+x", "let the owner execute it"),
            ("a+r", "let everyone read it"),
            ("go-w", "take write away from group and others"),
            ("+X", "execute on directories only"),
        ];
        let completions = modes
            .iter()
            .map(|(mode, desc)| {
                Completion::new((*mode).to_string(), CompletionKind::Other).with_desc(desc)
            })
            .collect();
        let ranked = rank_prefix_then_fuzzy(completions, prefix);
        if !ranked.is_empty() {
            return Some(ranked);
        }
    }

    // Options whose value is one of a fixed set. Both spellings: `-X POST`
    // and `--pretty=oneline`.
    if !prefix.starts_with('-') {
        if let Some(previous) = words.last().copied().filter(|word| word.starts_with('-')) {
            if let Some(values) = option_value_choices(cmd, previous) {
                let completions = values
                    .iter()
                    .map(|(value, desc)| {
                        Completion::new((*value).to_string(), CompletionKind::Other).with_desc(desc)
                    })
                    .collect();
                let ranked = rank_prefix_then_fuzzy(completions, prefix);
                if !ranked.is_empty() {
                    return Some(ranked);
                }
            }
        }
    }
    if let Some((option, value_prefix)) = prefix.split_once('=') {
        if let Some(values) = option_value_choices(cmd, option) {
            let completions = values
                .iter()
                .map(|(value, desc)| {
                    Completion::new((*value).to_string(), CompletionKind::Other).with_desc(desc)
                })
                .collect();
            let ranked: Vec<Completion> = rank_prefix_then_fuzzy(completions, value_prefix)
                .into_iter()
                .map(|mut completion| {
                    completion.text = format!("{option}={}", completion.text);
                    completion
                })
                .collect();
            if !ranked.is_empty() {
                return Some(ranked);
            }
        }
    }

    // Version managers keep their interpreters in a fixed directory, so the
    // versions installed here can be offered without running anything.
    if matches!(cmd, "nvm" | "pyenv" | "rbenv" | "jenv") && !prefix.starts_with('-') {
        let takes_version = word_count == 1
            || matches!(
                words.get(1).copied().unwrap_or(""),
                "use"
                    | "install"
                    | "uninstall"
                    | "exec"
                    | "global"
                    | "local"
                    | "shell"
                    | "which"
                    | "prefix"
                    | "alias"
                    | "run"
            );
        if takes_version {
            let results = rank_prefix_then_fuzzy(complete_toolchain_versions(cmd, state), prefix);
            if !results.is_empty() {
                return Some(results);
            }
        }
    }

    // `source <TAB>` next to a virtual environment: its activate script is
    // three directories down, and it is almost always what was meant.
    if matches!(cmd, "source" | ".") && !prefix.starts_with('-') {
        let mut results = complete_venv_activators(state);
        results.retain(|completion| completion.text.starts_with(prefix));
        if !results.is_empty() {
            let mut rest = complete_path(prefix, state);
            rest.retain(|candidate| !results.iter().any(|item| item.text == candidate.text));
            results.extend(filter_by_file_type(rest, cmd));
            return Some(results);
        }
    }

    // jsh's own builtins know their arguments exactly; nothing here needs a
    // probe or a spec file.
    if cmd == "shopt" && !prefix.starts_with('-') {
        let completions = crate::builtins::SHOPT_OPTIONS
            .iter()
            .map(|(name, default_on)| {
                Completion::new((*name).to_string(), CompletionKind::Other).with_desc(
                    if *default_on {
                        "on by default"
                    } else {
                        "off by default"
                    },
                )
            })
            .collect();
        return Some(rank_prefix_then_fuzzy(completions, prefix));
    }

    if cmd == "set" && matches!(words.last().copied(), Some("-o" | "+o")) {
        let completions = crate::builtins::SET_OPTIONS
            .iter()
            .map(|(name, flag)| {
                let completion = Completion::new((*name).to_string(), CompletionKind::Other);
                match flag {
                    Some(flag) => completion.with_desc(&format!("same as -{flag}")),
                    None => completion,
                }
            })
            .collect();
        return Some(rank_prefix_then_fuzzy(completions, prefix));
    }

    if cmd == "hook" && word_count >= 2 {
        let hook_sub = words.get(1).copied().unwrap_or("");
        // `hook add <kind> <function>` / `hook remove <kind> <function>`.
        if matches!(hook_sub, "add" | "remove") && word_count == 2 {
            let kinds = [
                ("precmd", "before each prompt"),
                ("preexec", "before each command runs"),
                ("chpwd", "after the directory changes"),
            ];
            let completions = kinds
                .iter()
                .map(|(name, desc)| {
                    Completion::new((*name).to_string(), CompletionKind::Subcommand).with_desc(desc)
                })
                .collect();
            return Some(rank_prefix_then_fuzzy(completions, prefix));
        }
        if matches!(hook_sub, "add" | "remove") && word_count == 3 {
            // Registered hooks for `remove`, every function for `add`.
            let kind = words.get(2).copied().unwrap_or("");
            let registered: &[String] = match kind {
                "precmd" => &state.hooks.precmd,
                "preexec" => &state.hooks.preexec,
                "chpwd" => &state.hooks.chpwd,
                _ => &[],
            };
            let completions: Vec<Completion> = if hook_sub == "remove" {
                registered
                    .iter()
                    .map(|name| {
                        Completion::new(name.clone(), CompletionKind::Function)
                            .with_desc(&format!("registered {kind} hook"))
                    })
                    .collect()
            } else {
                let mut names: Vec<&String> = state.functions.keys().collect();
                names.sort();
                names
                    .into_iter()
                    .map(|name| {
                        Completion::new(name.clone(), CompletionKind::Function)
                            .with_desc("function")
                    })
                    .collect()
            };
            return Some(rank_prefix_then_fuzzy(completions, prefix));
        }
    }

    if matches!(cmd, "workflow" | "wf") && !prefix.starts_with('-') {
        let completions: Vec<Completion> = state
            .workflow_registry
            .search("")
            .into_iter()
            .map(|workflow| {
                Completion::new(workflow.name.clone(), CompletionKind::Other)
                    .with_desc(&workflow.description)
            })
            .collect();
        let ranked = rank_prefix_then_fuzzy(completions, prefix);
        if !ranked.is_empty() {
            return Some(ranked);
        }
    }

    // kubectl names that live in the local kubeconfig. Resource names would
    // need the API server, and a keystroke must not become a network call.
    if matches!(cmd, "kubectl" | "kubectx" | "kubens" | "helm" | "k9s") {
        let namespace_flag = matches!(words.last().copied(), Some("-n" | "--namespace"));
        if namespace_flag && !prefix.starts_with('-') {
            let results =
                rank_prefix_then_fuzzy(complete_kube_names("", KubeName::Namespace, state), prefix);
            if !results.is_empty() {
                return Some(results);
            }
        }
        if matches!(words.last().copied(), Some("--context")) && !prefix.starts_with('-') {
            let results =
                rank_prefix_then_fuzzy(complete_kube_names("", KubeName::Context, state), prefix);
            if !results.is_empty() {
                return Some(results);
            }
        }
        for (flag, kind) in [
            ("--namespace=", KubeName::Namespace),
            ("--context=", KubeName::Context),
            ("--cluster=", KubeName::Cluster),
            ("--user=", KubeName::User),
        ] {
            if let Some(value) = prefix.strip_prefix(flag) {
                let results: Vec<Completion> =
                    rank_prefix_then_fuzzy(complete_kube_names("", kind, state), value)
                        .into_iter()
                        .map(|mut completion| {
                            completion.text = format!("{flag}{}", completion.text);
                            completion
                        })
                        .collect();
                if !results.is_empty() {
                    return Some(results);
                }
            }
        }
    }

    // `kubectl config use-context <TAB>` and the sibling subcommands.
    if cmd == "kubectl" && words.get(1) == Some(&"config") && word_count >= 3 {
        let kind = match words.get(2).copied().unwrap_or("") {
            "use-context" | "delete-context" | "rename-context" => Some(KubeName::Context),
            "delete-cluster" => Some(KubeName::Cluster),
            "delete-user" => Some(KubeName::User),
            _ => None,
        };
        if let Some(kind) = kind {
            let results = rank_prefix_then_fuzzy(complete_kube_names("", kind, state), prefix);
            if !results.is_empty() {
                return Some(results);
            }
        }
    }

    // `kubectx <TAB>` switches contexts; `kubens <TAB>` switches namespaces.
    if matches!(cmd, "kubectx" | "kubens") && word_count == 1 && !prefix.starts_with('-') {
        let kind = if cmd == "kubectx" {
            KubeName::Context
        } else {
            KubeName::Namespace
        };
        let results = rank_prefix_then_fuzzy(complete_kube_names("", kind, state), prefix);
        if !results.is_empty() {
            return Some(results);
        }
    }

    // `trap 'handler' <TAB>`: the arguments after the handler are signals.
    if cmd == "trap" && word_count >= 2 && !prefix.starts_with('-') {
        let completions = TRAP_SIGNALS
            .iter()
            .map(|(signal, desc)| {
                Completion::new(signal.to_string(), CompletionKind::Other).with_desc(desc)
            })
            .collect();
        return Some(rank_prefix_then_fuzzy(completions, prefix));
    }

    // Unit names for systemctl, probed from systemd itself the bounded way.
    if cmd == "systemctl" && word_count >= 2 && !prefix.starts_with('-') {
        let subcmd = words[1..]
            .iter()
            .find(|w| !w.starts_with('-'))
            .copied()
            .unwrap_or("");
        let user_scope = words.contains(&"--user");
        // `start`/`enable`/`mask` operate on unit files, including units
        // systemd has not loaded; the rest act on loaded units.
        let from_unit_files = matches!(subcmd, "start" | "enable" | "mask" | "cat" | "edit");
        let known_unit_subcmd = from_unit_files
            || matches!(
                subcmd,
                "stop"
                    | "restart"
                    | "reload"
                    | "status"
                    | "disable"
                    | "unmask"
                    | "is-active"
                    | "is-enabled"
                    | "is-failed"
                    | "reset-failed"
                    | "show"
            );
        if known_unit_subcmd {
            let results = rank_prefix_then_fuzzy(
                complete_systemctl_units("", user_scope, from_unit_files),
                prefix,
            );
            if !results.is_empty() {
                return Some(results);
            }
        }
    }

    // `journalctl -u <TAB>` and `--unit=<TAB>` name loaded units too.
    if cmd == "journalctl" {
        let unit_flags = ["-u", "--unit", "--user-unit"];
        let prev = words.last().copied().unwrap_or("");
        if unit_flags.contains(&prev) && !prefix.starts_with('-') {
            let results = rank_prefix_then_fuzzy(
                complete_systemctl_units("", prev == "--user-unit", false),
                prefix,
            );
            if !results.is_empty() {
                return Some(results);
            }
        }
        for flag in ["--unit=", "--user-unit="] {
            if let Some(value) = prefix.strip_prefix(flag) {
                let results: Vec<Completion> = rank_prefix_then_fuzzy(
                    complete_systemctl_units("", flag == "--user-unit=", false),
                    value,
                )
                .into_iter()
                .map(|mut completion| {
                    completion.text = format!("{flag}{}", completion.text);
                    completion
                })
                .collect();
                if !results.is_empty() {
                    return Some(results);
                }
            }
        }
    }

    // Owner and group arguments from /etc/passwd and /etc/group.
    if !prefix.starts_with('-') {
        let first_positional = !words[1..].iter().any(|w| !w.starts_with('-'));
        if cmd == "chown" && first_positional {
            // `user:group` completes the group half once the colon is typed.
            if let Some((user, group_prefix)) = prefix.split_once(':') {
                return Some(
                    rank_prefix_then_fuzzy(complete_groups(""), group_prefix)
                        .into_iter()
                        .map(|mut completion| {
                            completion.text = format!("{user}:{}", completion.text);
                            completion
                        })
                        .collect(),
                );
            }
            let results = rank_prefix_then_fuzzy(complete_users(""), prefix);
            if !results.is_empty() {
                return Some(results);
            }
        }
        if cmd == "chgrp" && first_positional {
            let results = rank_prefix_then_fuzzy(complete_groups(""), prefix);
            if !results.is_empty() {
                return Some(results);
            }
        }
        if matches!(cmd, "su" | "passwd" | "id" | "groups") && first_positional {
            let results = rank_prefix_then_fuzzy(complete_users(""), prefix);
            if !results.is_empty() {
                return Some(results);
            }
        }
    }

    // Option completions for common commands
    if prefix.starts_with('-') {
        let options: &[(&str, &str)] = match cmd {
            "ls" => &[
                ("-l", "long format"),
                ("-a", "include hidden"),
                ("-h", "human readable"),
                ("-r", "reverse order"),
                ("-t", "sort by time"),
                ("-S", "sort by size"),
                ("-R", "recursive"),
                ("-d", "list directories"),
            ],
            "grep" => &[
                ("-i", "case insensitive"),
                ("-v", "invert match"),
                ("-n", "show line numbers"),
                ("-r", "recursive"),
                ("-R", "recursive dereference"),
                ("-l", "list filenames"),
                ("-c", "count matches"),
                ("-o", "only matching parts"),
                ("-E", "extended regex"),
                ("-F", "fixed strings"),
            ],
            "find" => &[
                ("-type", "file type"),
                ("-name", "filename pattern"),
                ("-iname", "case insensitive name"),
                ("-path", "path pattern"),
                ("-regex", "regex pattern"),
                ("-size", "file size"),
                ("-mtime", "modification time"),
                ("-atime", "access time"),
                ("-user", "file owner"),
                ("-exec", "execute command"),
            ],
            "tar" => &[
                ("-c", "create archive"),
                ("-x", "extract archive"),
                ("-t", "list contents"),
                ("-v", "verbose"),
                ("-z", "gzip compression"),
                ("-j", "bzip2 compression"),
                ("-f", "archive file"),
                ("-C", "change directory"),
            ],
            "rm" => &[
                ("-r", "recursive"),
                ("-f", "force"),
                ("-i", "interactive"),
                ("-v", "verbose"),
            ],
            "cp" => &[
                ("-r", "recursive"),
                ("-i", "interactive"),
                ("-v", "verbose"),
                ("-a", "preserve all"),
                ("-p", "preserve properties"),
            ],
            "mkdir" => &[("-p", "create parents"), ("-m", "mode"), ("-v", "verbose")],
            "chmod" => &[
                ("-R", "recursive"),
                ("-v", "verbose"),
                ("-c", "report changes only"),
                ("--reference", "copy another file's mode"),
            ],
            _ => COMMON_FLAGS
                .iter()
                .find(|(name, _)| *name == cmd)
                .map(|(_, flags)| *flags)
                .unwrap_or(&[]),
        };

        if !options.is_empty() {
            let completions = options
                .iter()
                .map(|(opt, desc)| {
                    Completion::new((*opt).to_string(), CompletionKind::Flag).with_desc(desc)
                })
                .collect::<Vec<_>>();
            let ranked = rank_prefix_then_fuzzy(completions, prefix);
            if !ranked.is_empty() {
                return Some(ranked);
            }
        }
    }

    // First-level subcommands with descriptions
    if word_count == 1 && !prefix.starts_with('-') {
        let subs: &[(&str, &str)] = match cmd {
            "git" => &[
                ("add", "Stage changes"),
                ("bisect", "Binary search for bugs"),
                ("blame", "Show line annotations"),
                ("branch", "List/create branches"),
                ("checkout", "Switch branches/restore files"),
                ("cherry-pick", "Apply commit changes"),
                ("clone", "Clone a repository"),
                ("commit", "Record changes"),
                ("config", "Get/set configuration"),
                ("diff", "Show changes"),
                ("fetch", "Download objects/refs"),
                ("grep", "Search tracked files"),
                ("init", "Create empty repository"),
                ("log", "Show commit log"),
                ("merge", "Join branches"),
                ("mv", "Move/rename files"),
                ("pull", "Fetch and merge"),
                ("push", "Update remote refs"),
                ("rebase", "Reapply commits"),
                ("remote", "Manage remotes"),
                ("reset", "Reset HEAD"),
                ("restore", "Restore working tree"),
                ("revert", "Revert commits"),
                ("rm", "Remove files"),
                ("show", "Show objects"),
                ("stash", "Stash changes"),
                ("status", "Show working tree status"),
                ("switch", "Switch branches"),
                ("tag", "Manage tags"),
                ("worktree", "Manage worktrees"),
            ],
            "cargo" => &[
                ("add", "Add dependency"),
                ("bench", "Run benchmarks"),
                ("build", "Compile project"),
                ("check", "Check for errors"),
                ("clean", "Remove artifacts"),
                ("clippy", "Run linter"),
                ("doc", "Build documentation"),
                ("fetch", "Fetch dependencies"),
                ("fix", "Auto-fix warnings"),
                ("fmt", "Format code"),
                ("init", "Init in existing dir"),
                ("install", "Install binary"),
                ("new", "Create new project"),
                ("publish", "Publish to crates.io"),
                ("remove", "Remove dependency"),
                ("run", "Run binary"),
                ("search", "Search crates.io"),
                ("test", "Run tests"),
                ("tree", "Show dependency tree"),
                ("uninstall", "Remove binary"),
                ("update", "Update dependencies"),
            ],
            "docker" => &[
                ("build", "Build image"),
                ("compose", "Multi-container apps"),
                ("container", "Manage containers"),
                ("cp", "Copy files"),
                ("create", "Create container"),
                ("exec", "Run in container"),
                ("image", "Manage images"),
                ("images", "List images"),
                ("kill", "Kill container"),
                ("logs", "View logs"),
                ("network", "Manage networks"),
                ("ps", "List containers"),
                ("pull", "Pull image"),
                ("push", "Push image"),
                ("restart", "Restart container"),
                ("rm", "Remove container"),
                ("rmi", "Remove image"),
                ("run", "Create and run"),
                ("start", "Start container"),
                ("stop", "Stop container"),
                ("tag", "Tag image"),
                ("volume", "Manage volumes"),
            ],
            "systemctl" => &[
                ("daemon-reload", "Reload unit files"),
                ("disable", "Disable unit"),
                ("edit", "Edit unit file"),
                ("enable", "Enable unit"),
                ("is-active", "Check if active"),
                ("is-enabled", "Check if enabled"),
                ("list-units", "List loaded units"),
                ("reload", "Reload unit"),
                ("restart", "Restart unit"),
                ("start", "Start unit"),
                ("status", "Show status"),
                ("stop", "Stop unit"),
            ],
            "npm" => &[
                ("audit", "Security audit"),
                ("build", "Build package"),
                ("cache", "Manage cache"),
                ("ci", "Clean install"),
                ("clean", "Clean project"),
                ("config", "Manage config"),
                ("create", "Create package"),
                ("exec", "Run package binary"),
                ("init", "Init package.json"),
                ("install", "Install packages"),
                ("link", "Symlink package"),
                ("list", "List installed"),
                ("outdated", "Check outdated"),
                ("pack", "Create tarball"),
                ("publish", "Publish package"),
                ("rebuild", "Rebuild native"),
                ("remove", "Remove package"),
                ("run", "Run script"),
                ("search", "Search registry"),
                ("start", "Start script"),
                ("test", "Run tests"),
                ("uninstall", "Uninstall package"),
                ("update", "Update packages"),
                ("version", "Bump version"),
            ],
            "hook" => &[
                ("add", "Add hook"),
                ("remove", "Remove hook"),
                ("list", "List hooks"),
            ],
            "bookmark" => &[
                ("add", "Add bookmark"),
                ("go", "Go to bookmark"),
                ("ls", "List bookmarks"),
                ("rm", "Remove bookmark"),
            ],
            "kubectl" => &[
                ("apply", "Apply configuration"),
                ("attach", "Attach to container"),
                ("auth", "Check authorization"),
                ("config", "Modify kubeconfig"),
                ("create", "Create resource"),
                ("delete", "Delete resources"),
                ("describe", "Show resource details"),
                ("diff", "Diff configurations"),
                ("edit", "Edit resource"),
                ("exec", "Execute in container"),
                ("expose", "Expose as service"),
                ("get", "Display resources"),
                ("label", "Update labels"),
                ("logs", "Print container logs"),
                ("patch", "Patch resource"),
                ("port-forward", "Forward ports"),
                ("proxy", "Run API proxy"),
                ("rollout", "Manage rollouts"),
                ("run", "Run pod"),
                ("scale", "Scale replicas"),
                ("set", "Set resource fields"),
                ("top", "Resource usage"),
                ("version", "Print version"),
            ],
            "pip" | "pip3" => &[
                ("install", "Install packages"),
                ("uninstall", "Uninstall packages"),
                ("download", "Download packages"),
                ("freeze", "Output installed"),
                ("list", "List installed"),
                ("show", "Show package info"),
                ("search", "Search PyPI"),
                ("wheel", "Build wheels"),
                ("hash", "Compute hashes"),
                ("check", "Verify packages"),
                ("config", "Manage config"),
                ("cache", "Manage cache"),
            ],
            "go" => &[
                ("build", "Compile packages"),
                ("clean", "Remove objects"),
                ("doc", "Show documentation"),
                ("env", "Print environment"),
                ("fix", "Update packages"),
                ("fmt", "Format source"),
                ("generate", "Run go generate"),
                ("get", "Download modules"),
                ("install", "Compile and install"),
                ("list", "List packages"),
                ("mod", "Module maintenance"),
                ("run", "Compile and run"),
                ("test", "Run tests"),
                ("tool", "Run go tool"),
                ("version", "Print version"),
                ("vet", "Report issues"),
                ("work", "Workspace mode"),
            ],
            // Phase 14d: signature-driven first-arg completion for
            // `help <cmd>` — list every signed value-aware builtin.
            "help" => {
                let mut names: Vec<&'static str> =
                    crate::signature::SIGNATURES.keys().copied().collect();
                names.sort_unstable();
                let completions: Vec<Completion> = names
                    .into_iter()
                    .map(|n| {
                        let sig = crate::signature::SIGNATURES.get(n).unwrap();
                        Completion {
                            text: n.to_string(),
                            display: n.to_string(),
                            description: Some(sig.desc.to_string()),
                            kind: CompletionKind::Subcommand,
                            is_dir: false,
                        }
                    })
                    .collect();
                return Some(rank_prefix_then_fuzzy(completions, prefix));
            }
            // `error <subcmd>` — currently just `make`.
            "error" => {
                let subs = [("make", "Raise a structured error with a message")];
                let completions: Vec<Completion> = subs
                    .iter()
                    .filter(|(n, _)| n.starts_with(prefix))
                    .map(|(n, d)| Completion {
                        text: n.to_string(),
                        display: n.to_string(),
                        description: Some(d.to_string()),
                        kind: CompletionKind::Subcommand,
                        is_dir: false,
                    })
                    .collect();
                return Some(completions);
            }
            _ => return None,
        };

        let completions = subs
            .iter()
            .map(|(name, desc)| Completion {
                text: name.to_string(),
                display: name.to_string(),
                description: Some(desc.to_string()),
                kind: CompletionKind::Subcommand,
                is_dir: false,
            })
            .collect::<Vec<_>>();

        return Some(rank_prefix_then_fuzzy(completions, prefix));
    }

    // Second-level: git context-aware completions
    // Flags come from the richer JSON command spec below. Dynamic Git argument
    // completion must not swallow inputs such as `git push -` or `git switch -`.
    if cmd == "git" && word_count >= 2 && !prefix.starts_with('-') {
        let subcmd = words.get(1).copied().unwrap_or("");
        match subcmd {
            // Deleting a branch names a branch, never a tag or a remote ref,
            // and never the branch that is checked out — Git would refuse.
            "branch" | "switch"
                if words
                    .iter()
                    .skip(2)
                    .any(|word| matches!(*word, "-d" | "-D" | "--delete")) =>
            {
                let current = state.cached_git_branch.as_deref();
                let branches: Vec<Completion> = complete_git_local_branches("")
                    .into_iter()
                    .filter(|completion| Some(completion.text.as_str()) != current)
                    .collect();
                return Some(rank_prefix_then_fuzzy(branches, prefix));
            }
            "checkout" | "switch" | "merge" | "rebase" | "branch" | "diff" | "log" => {
                // All refs, prefix matches first, fuzzy when nothing starts
                // with the typed text (`git checkout rel21` → release-2.1).
                return Some(rank_prefix_then_fuzzy(complete_git_refs(""), prefix));
            }
            "config" => {
                // `--get`/`--unset` name a key that exists here; otherwise
                // the well-known keys are what someone is reaching for.
                let existing = words
                    .iter()
                    .skip(2)
                    .any(|word| matches!(*word, "--get" | "--get-all" | "--unset" | "--unset-all"));
                let mut results = if existing {
                    complete_git_config_keys("")
                } else {
                    Vec::new()
                };
                if results.is_empty() {
                    results = complete_git_config_keys("");
                    for (key, description) in GIT_CONFIG_KEYS {
                        if !results.iter().any(|item| item.text == *key) {
                            results.push(
                                Completion::new((*key).to_string(), CompletionKind::Other)
                                    .with_desc(description),
                            );
                        }
                    }
                }
                return Some(rank_prefix_then_fuzzy(results, prefix));
            }
            "worktree" if word_count == 2 => {
                let subs = [
                    ("add", "Create a worktree"),
                    ("list", "List worktrees"),
                    ("lock", "Prevent pruning"),
                    ("move", "Move a worktree"),
                    ("prune", "Remove stale entries"),
                    ("remove", "Remove a worktree"),
                    ("repair", "Repair worktree links"),
                    ("unlock", "Allow pruning"),
                ];
                let completions = subs
                    .iter()
                    .map(|(name, desc)| {
                        Completion::new((*name).to_string(), CompletionKind::Subcommand)
                            .with_desc(desc)
                    })
                    .collect();
                return Some(rank_prefix_then_fuzzy(completions, prefix));
            }
            "worktree" if word_count >= 3 => {
                if matches!(
                    words.get(2).copied().unwrap_or(""),
                    "remove" | "lock" | "unlock" | "move" | "repair"
                ) {
                    let results = rank_prefix_then_fuzzy(complete_git_worktrees(""), prefix);
                    if !results.is_empty() {
                        return Some(results);
                    }
                }
            }
            "tag"
                if words
                    .iter()
                    .skip(2)
                    .any(|word| matches!(*word, "-d" | "--delete" | "-v" | "--verify")) =>
            {
                let results = rank_prefix_then_fuzzy(complete_git_tags(""), prefix);
                if !results.is_empty() {
                    return Some(results);
                }
            }
            "add" => {
                return Some(rank_prefix_then_fuzzy(
                    complete_git_dirty_files("", "add"),
                    prefix,
                ));
            }
            "restore" => {
                if matches!(words.last(), Some(&"--source" | &"-s")) {
                    return Some(complete_git_refs(prefix));
                }
                let context = if words
                    .iter()
                    .skip(2)
                    .any(|word| *word == "--staged" || *word == "-S")
                {
                    "restore_staged"
                } else {
                    "restore"
                };
                return Some(rank_prefix_then_fuzzy(
                    complete_git_dirty_files("", context),
                    prefix,
                ));
            }
            "reset" => {
                let mut results = complete_git_refs("");
                results.extend(complete_git_dirty_files("", "reset"));
                return Some(rank_prefix_then_fuzzy(results, prefix));
            }
            "stash" if word_count == 2 => {
                // stash subcommands
                let subs = &[
                    ("push", "Stash changes"),
                    ("pop", "Apply and drop"),
                    ("apply", "Apply stash"),
                    ("drop", "Drop stash"),
                    ("list", "List stashes"),
                    ("show", "Show stash"),
                    ("clear", "Clear all stashes"),
                ];
                let completions = subs
                    .iter()
                    .map(|(name, desc)| Completion {
                        text: name.to_string(),
                        display: name.to_string(),
                        description: Some(desc.to_string()),
                        kind: CompletionKind::Subcommand,
                        is_dir: false,
                    })
                    .collect();
                return Some(rank_prefix_then_fuzzy(completions, prefix));
            }
            "stash" if word_count >= 3 => {
                let stash_sub = words.get(2).copied().unwrap_or("");
                if stash_sub == "pop"
                    || stash_sub == "apply"
                    || stash_sub == "drop"
                    || stash_sub == "show"
                {
                    return Some(complete_git_stashes(prefix));
                }
            }
            "cherry-pick" | "revert" => {
                return Some(complete_git_recent_commits(prefix));
            }
            "remote" if word_count == 2 => {
                let subs = &[
                    ("add", "Add remote"),
                    ("remove", "Remove remote"),
                    ("rename", "Rename remote"),
                    ("show", "Show remote"),
                    ("prune", "Prune stale refs"),
                    ("update", "Fetch updates"),
                ];
                let completions = subs
                    .iter()
                    .map(|(name, desc)| Completion {
                        text: name.to_string(),
                        display: name.to_string(),
                        description: Some(desc.to_string()),
                        kind: CompletionKind::Subcommand,
                        is_dir: false,
                    })
                    .collect();
                return Some(rank_prefix_then_fuzzy(completions, prefix));
            }
            "remote" if word_count >= 3 => {
                return Some(rank_prefix_then_fuzzy(complete_git_remotes(""), prefix));
            }
            "push" | "pull" | "fetch" if word_count == 2 => {
                let mut results = complete_git_remotes(prefix);
                if let Some(remote) = state.cached_git_remote.as_deref() {
                    promote_git_context(&mut results, remote, prefix, "tracking remote");
                }
                return Some(results);
            }
            "push" | "pull" | "fetch" if word_count >= 3 => {
                let mut results = complete_git_refs(prefix);
                if let Some(branch) = state.cached_git_branch.as_deref() {
                    promote_git_context(&mut results, branch, prefix, "current branch");
                }
                return Some(results);
            }
            _ => {}
        }
    }

    // Second-level: docker compose subcommands
    if cmd == "docker" && word_count == 2 {
        let subcmd = words.get(1).copied().unwrap_or("");
        if subcmd == "compose" {
            let subs = &[
                "build", "config", "create", "down", "events", "exec", "images", "kill", "logs",
                "ls", "pause", "port", "ps", "pull", "push", "restart", "rm", "run", "start",
                "stop", "top", "unpause", "up",
            ];
            let completions = subs
                .iter()
                .map(|s| Completion {
                    text: s.to_string(),
                    display: s.to_string(),
                    description: None,
                    kind: CompletionKind::Subcommand,
                    is_dir: false,
                })
                .collect::<Vec<_>>();
            return Some(rank_prefix_then_fuzzy(completions, prefix));
        }
    }

    // `docker compose <sub> <TAB>` and `docker-compose <sub> <TAB>` name
    // services from the project's own compose file.
    {
        let compose_sub = if cmd == "docker" && words.get(1) == Some(&"compose") {
            words.get(2).copied()
        } else if cmd == "docker-compose" {
            words.get(1).copied()
        } else {
            None
        };
        if let Some(compose_sub) = compose_sub {
            let takes_services = matches!(
                compose_sub,
                "up" | "down"
                    | "start"
                    | "stop"
                    | "restart"
                    | "logs"
                    | "exec"
                    | "run"
                    | "build"
                    | "pull"
                    | "push"
                    | "ps"
                    | "rm"
                    | "kill"
                    | "pause"
                    | "unpause"
                    | "top"
                    | "images"
                    | "port"
                    | "create"
                    | "config"
            );
            // `exec`/`run` take one service, then a command inside it.
            let single_service = matches!(compose_sub, "exec" | "run");
            let service_index = if cmd == "docker" { 3 } else { 2 };
            let service_named = words
                .iter()
                .skip(service_index)
                .any(|word| !word.starts_with('-'));
            if takes_services && !prefix.starts_with('-') && !(single_service && service_named) {
                let results = rank_prefix_then_fuzzy(complete_compose_services(""), prefix);
                if !results.is_empty() {
                    return Some(results);
                }
            }
        }
    }

    // Second-level: docker container and image names from the local daemon,
    // probed the same bounded way Git arguments are.
    if cmd == "docker" && word_count >= 2 && !prefix.starts_with('-') {
        // `docker container stop` and `docker image rm` name the same targets
        // one level deeper.
        let (subcmd, target_index) = match words.get(1).copied().unwrap_or("") {
            "container" | "image" if word_count >= 3 => (words.get(2).copied().unwrap_or(""), 3),
            other => (other, 2),
        };
        let running_only = matches!(
            subcmd,
            "exec"
                | "attach"
                | "stop"
                | "restart"
                | "kill"
                | "pause"
                | "unpause"
                | "top"
                | "port"
                | "stats"
                | "update"
        );
        let any_state = matches!(
            subcmd,
            "start"
                | "rm"
                | "logs"
                | "inspect"
                | "cp"
                | "commit"
                | "wait"
                | "diff"
                | "export"
                | "rename"
        );
        let image_arg = matches!(subcmd, "rmi" | "run" | "push" | "tag" | "history" | "save");
        // `docker exec app bash -c <TAB>`: once the single target is named,
        // the rest belongs to the command inside the container.
        let single_target = matches!(subcmd, "exec" | "attach" | "run");
        let target_named = words
            .iter()
            .skip(target_index)
            .any(|word| !word.starts_with('-'));
        if (running_only || any_state || image_arg) && !(single_target && target_named) {
            let results = if image_arg {
                rank_prefix_then_fuzzy(complete_docker_images(""), prefix)
            } else {
                rank_prefix_then_fuzzy(complete_docker_containers("", any_state), prefix)
            };
            if !results.is_empty() {
                return Some(results);
            }
        }
    }

    // Second-level: bookmark name completion for go/rm
    if cmd == "bookmark" && word_count == 2 {
        let subcmd = words.get(1).copied().unwrap_or("");
        if subcmd == "go" || subcmd == "rm" {
            if let Ok(db) = crate::bookmarks::get_bookmark_db().lock() {
                let completions = db
                    .names()
                    .into_iter()
                    .map(|n| Completion {
                        text: n.clone(),
                        display: n,
                        description: Some("bookmark".to_string()),
                        kind: CompletionKind::Other,
                        is_dir: false,
                    })
                    .collect::<Vec<_>>();
                return Some(rank_prefix_then_fuzzy(completions, prefix));
            }
        }
    }

    None
}

/// Value-aware builtins whose first argument names a field of the records
/// flowing through the pipeline.
const FIELD_TAKING_BUILTINS: &[&str] = &[
    "where", "select", "sort-by", "group-by", "reject", "get", "uniq-by", "count-by", "flatten",
    "rename",
];

/// Field names for a value-aware builtin, read from the structured file the
/// pipeline started with.
///
/// This is the one completion that has to look left past the pipe: `where`
/// alone says nothing, while `from-json orders.json | where <TAB>` knows
/// exactly which fields exist. The file is parsed, never executed, and only
/// when an earlier stage names one — a pipeline fed by a command's output
/// would have to run that command to know, which a keystroke must not do.
fn pipeline_field_completions(
    cmd: &str,
    prefix: &str,
    buf: &str,
    word_start: usize,
    state: &ShellState,
) -> Option<Vec<Completion>> {
    if !FIELD_TAKING_BUILTINS.contains(&cmd) || prefix.starts_with('-') {
        return None;
    }
    // Only the first argument of the builtin is a field name.
    let segment_start = active_command_segment_start(&buf[..word_start]);
    let words = buf[segment_start..word_start].split_whitespace().count();
    if words > 1 {
        return None;
    }

    let fields = structured_source_fields(&buf[..segment_start], state)?;
    let completions: Vec<Completion> = fields
        .into_iter()
        .map(|(name, kind)| Completion::new(name, CompletionKind::Other).with_desc(&kind))
        .collect();
    let ranked = rank_prefix_then_fuzzy(completions, prefix);
    (!ranked.is_empty()).then_some(ranked)
}

/// Find the structured file an earlier pipeline stage reads, and return its
/// record fields with the type each holds.
fn structured_source_fields(pipeline: &str, state: &ShellState) -> Option<Vec<(String, String)>> {
    // The last reader wins: `from-json a.json | ... | from-csv b.csv |`.
    let mut source: Option<(&str, String)> = None;
    for stage in pipeline.split('|') {
        let mut words = stage.split_whitespace();
        let Some(reader) = words.next() else { continue };
        if !matches!(
            reader,
            "from-json" | "from-csv" | "from-yaml" | "from-toml" | "from-ndjson" | "open"
        ) {
            continue;
        }
        // The argument may be a redirection or a flag; take the first plain
        // word, which is where the file name goes.
        let Some(argument) = words.find(|word| !word.starts_with('-')) else {
            continue;
        };
        source = Some((reader, unescape_shell_word(argument)));
    }
    let (reader, argument) = source?;

    let path = expand_completion_prefix(&argument, state)?;
    let content =
        crate::io_guard::read_regular_text(Path::new(&path), MAX_COMPLETION_PROJECT_FILE_BYTES)
            .ok()?;
    let format = if reader == "open" {
        Path::new(&path).extension()?.to_str()?
    } else {
        reader.trim_start_matches("from-")
    };
    structured_fields(&content, format)
}

/// Field names and value types from one structured document.
fn structured_fields(content: &str, format: &str) -> Option<Vec<(String, String)>> {
    let value: serde_json::Value = match format {
        "json" => serde_json::from_str(content).ok()?,
        "ndjson" | "jsonl" => {
            let first = content.lines().find(|line| !line.trim().is_empty())?;
            serde_json::from_str(first).ok()?
        }
        "yaml" | "yml" => {
            let parsed: serde_yaml::Value = serde_yaml::from_str(content).ok()?;
            serde_json::to_value(parsed).ok()?
        }
        "toml" => {
            // The same conversion `from-toml` itself uses, so a field named
            // here is the field that pipeline would carry.
            let parsed: toml::Value = toml::from_str(content).ok()?;
            crate::value_builtins::toml_to_json(parsed)
        }
        "csv" => {
            // The header row names the fields; the first data row types them.
            let mut lines = content.lines().filter(|line| !line.trim().is_empty());
            let header = lines.next()?;
            let sample: Vec<&str> = lines
                .next()
                .map(|row| row.split(',').collect())
                .unwrap_or_default();
            return Some(
                header
                    .split(',')
                    .enumerate()
                    .map(|(index, name)| {
                        let name = name.trim().trim_matches('"').to_string();
                        let kind = sample
                            .get(index)
                            .map(|value| csv_value_kind(value.trim()))
                            .unwrap_or("column");
                        (name, kind.to_string())
                    })
                    .filter(|(name, _)| !name.is_empty())
                    .take(MAX_COMPLETION_ITEMS)
                    .collect(),
            );
        }
        _ => return None,
    };

    // A list of records is the common shape; a bare record works too.
    let record = match &value {
        serde_json::Value::Array(items) => items.iter().find(|item| item.is_object())?,
        other => other,
    };
    let object = record.as_object()?;
    Some(
        object
            .iter()
            .map(|(name, value)| (name.clone(), json_value_kind(value).to_string()))
            .take(MAX_COMPLETION_ITEMS)
            .collect(),
    )
}

fn json_value_kind(value: &serde_json::Value) -> &'static str {
    match value {
        serde_json::Value::Null => "null",
        serde_json::Value::Bool(_) => "bool",
        serde_json::Value::Number(number) if number.is_i64() || number.is_u64() => "int",
        serde_json::Value::Number(_) => "float",
        serde_json::Value::String(_) => "string",
        serde_json::Value::Array(_) => "list",
        serde_json::Value::Object(_) => "record",
    }
}

fn csv_value_kind(value: &str) -> &'static str {
    let value = value.trim_matches('"');
    if value.is_empty() {
        "column"
    } else if value.parse::<i64>().is_ok() {
        "int"
    } else if value.parse::<f64>().is_ok() {
        "float"
    } else if matches!(value, "true" | "false") {
        "bool"
    } else {
        "string"
    }
}

/// The operators of `test`, `[` and `[[`, which are a closed set and the
/// place single letters are hardest to recall.
const TEST_OPERATORS: &[(&str, &str)] = &[
    ("-e", "the path exists"),
    ("-f", "a regular file"),
    ("-d", "a directory"),
    ("-L", "a symbolic link"),
    ("-h", "a symbolic link"),
    ("-s", "exists and is not empty"),
    ("-r", "readable"),
    ("-w", "writable"),
    ("-x", "executable"),
    ("-p", "a named pipe"),
    ("-S", "a socket"),
    ("-b", "a block device"),
    ("-c", "a character device"),
    ("-t", "a terminal on this descriptor"),
    ("-z", "the string is empty"),
    ("-n", "the string is not empty"),
    ("-eq", "numbers are equal"),
    ("-ne", "numbers differ"),
    ("-lt", "less than"),
    ("-le", "less than or equal"),
    ("-gt", "greater than"),
    ("-ge", "greater than or equal"),
    ("-nt", "newer than"),
    ("-ot", "older than"),
    ("-ef", "the same file"),
    ("-v", "the variable is set"),
    ("-a", "and"),
    ("-o", "or"),
];

/// Values an option accepts, when they are a fixed set worth choosing from.
/// Keyed by command and option name; `None` when the option takes free text,
/// a path, or something only the command itself could enumerate.
fn option_value_choices(
    cmd: &str,
    option: &str,
) -> Option<&'static [(&'static str, &'static str)]> {
    let choices: &[(&str, &str)] = match (cmd, option) {
        ("curl", "-X" | "--request") => &[
            ("GET", "read a resource"),
            ("POST", "create or submit"),
            ("PUT", "replace"),
            ("PATCH", "modify"),
            ("DELETE", "remove"),
            ("HEAD", "headers only"),
            ("OPTIONS", "what the resource allows"),
        ],
        ("git", "--pretty" | "--format") => &[
            ("oneline", "one commit per line"),
            ("short", "author and message"),
            ("medium", "the default"),
            ("full", "author and committer"),
            ("fuller", "with dates"),
            ("reference", "hash, subject and date"),
            ("raw", "the commit object"),
        ],
        ("git", "--color" | "--decorate")
        | ("ls" | "grep" | "diff" | "dir", "--color")
        | ("systemctl" | "journalctl", "--legend") => &[
            ("auto", "when the output is a terminal"),
            ("always", "even when piped"),
            ("never", "never"),
        ],
        ("git", "--strategy" | "-s") => &[
            ("ort", "the default merge strategy"),
            ("recursive", "the previous default"),
            ("resolve", "three-way merge"),
            ("ours", "keep this side"),
            ("subtree", "merge a subtree"),
        ],
        ("git", "--cleanup") => &[
            ("strip", "drop comments and blank lines"),
            ("whitespace", "strip trailing whitespace"),
            ("verbatim", "keep the message as written"),
            ("scissors", "cut at the scissor line"),
        ],
        ("systemctl", "--state") => &[
            ("active", "running or otherwise active"),
            ("inactive", "not active"),
            ("failed", "failed units"),
            ("enabled", "enabled unit files"),
            ("disabled", "disabled unit files"),
            ("static", "no enable symlinks"),
            ("masked", "cannot be started"),
        ],
        ("systemctl", "--type" | "-t") => &[
            ("service", "services"),
            ("socket", "sockets"),
            ("timer", "timers"),
            ("target", "targets"),
            ("mount", "mounts"),
            ("path", "path units"),
            ("device", "devices"),
        ],
        ("journalctl", "-p" | "--priority") => &[
            ("emerg", "system is unusable"),
            ("alert", "act immediately"),
            ("crit", "critical"),
            ("err", "errors"),
            ("warning", "warnings"),
            ("notice", "normal but significant"),
            ("info", "informational"),
            ("debug", "debug"),
        ],
        ("journalctl", "-o" | "--output") => &[
            ("short", "the default"),
            ("short-iso", "ISO timestamps"),
            ("json", "one JSON object per entry"),
            ("json-pretty", "indented JSON"),
            ("cat", "message text only"),
            ("verbose", "every field"),
        ],
        ("docker", "--restart") => &[
            ("no", "never restart"),
            ("on-failure", "restart on a non-zero exit"),
            ("always", "always restart"),
            ("unless-stopped", "unless it was stopped by hand"),
        ],
        ("docker" | "docker-compose", "--format") => {
            &[("json", "JSON output"), ("table", "a table")]
        }
        ("kubectl", "-o" | "--output") => &[
            ("wide", "extra columns"),
            ("json", "JSON"),
            ("yaml", "YAML"),
            ("name", "resource names only"),
            ("jsonpath", "a JSONPath expression"),
            ("custom-columns", "columns you name"),
        ],
        ("find", "-type") => &[
            ("f", "regular file"),
            ("d", "directory"),
            ("l", "symbolic link"),
            ("s", "socket"),
            ("p", "named pipe"),
            ("b", "block device"),
            ("c", "character device"),
        ],
        ("ps", "-o" | "--format") => &[
            ("pid", "process id"),
            ("ppid", "parent process id"),
            ("user", "owner"),
            ("comm", "command name"),
            ("args", "full command line"),
            ("pcpu", "CPU percentage"),
            ("pmem", "memory percentage"),
            ("rss", "resident memory"),
            ("etime", "elapsed time"),
            ("stat", "process state"),
        ],
        ("tar", "--format") => &[
            ("gnu", "GNU tar format"),
            ("pax", "POSIX pax format"),
            ("ustar", "POSIX ustar format"),
        ],
        ("cargo", "--message-format") => &[
            ("human", "for reading"),
            ("json", "for tools"),
            ("short", "one line per diagnostic"),
        ],
        ("npm" | "pnpm" | "yarn", "--save") => &[
            ("prod", "into dependencies"),
            ("dev", "into devDependencies"),
            ("optional", "into optionalDependencies"),
        ],
        ("ssh" | "scp" | "sftp", "-o") => &[
            ("StrictHostKeyChecking=", "trust policy for host keys"),
            ("UserKnownHostsFile=", "where host keys are stored"),
            ("ProxyJump=", "connect through another host"),
            ("ConnectTimeout=", "seconds before giving up"),
            ("ServerAliveInterval=", "keepalive interval"),
            ("ForwardAgent=", "forward the agent"),
        ],
        _ => return None,
    };
    Some(choices)
}

/// Flags for the tools people reach for constantly, where remembering which
/// letter means what is the actual friction. Kept to the options worth
/// choosing from a list: exhaustive coverage belongs in a spec file, and the
/// history fallback already recalls whatever else someone has typed before.
#[allow(clippy::type_complexity)]
const COMMON_FLAGS: &[(&str, &[(&str, &str)])] = &[
    (
        "ps",
        &[
            ("aux", "every process, user-oriented"),
            ("-e", "every process"),
            ("-f", "full format"),
            ("-o", "choose output columns"),
            ("--sort", "sort by a column"),
            ("-p", "by process id"),
            ("-u", "by user"),
        ],
    ),
    (
        "df",
        &[
            ("-h", "human readable sizes"),
            ("-i", "inodes instead of blocks"),
            ("-T", "show filesystem type"),
            ("-x", "exclude a filesystem type"),
        ],
    ),
    (
        "du",
        &[
            ("-h", "human readable sizes"),
            ("-s", "summary per argument"),
            ("-d", "limit depth"),
            ("-a", "include files"),
            ("-x", "stay on one filesystem"),
            ("--exclude", "skip matching paths"),
        ],
    ),
    (
        "curl",
        &[
            ("-s", "silent"),
            ("-S", "show errors even when silent"),
            ("-L", "follow redirects"),
            ("-o", "write to a file"),
            ("-O", "write to the remote name"),
            ("-X", "request method"),
            ("-H", "add a header"),
            ("-d", "request body"),
            ("-F", "multipart form field"),
            ("-u", "credentials"),
            ("-i", "include response headers"),
            ("-I", "headers only"),
            ("-f", "fail on HTTP errors"),
            ("--json", "JSON body, headers set"),
            ("--retry", "retry a failed transfer"),
        ],
    ),
    (
        "wget",
        &[
            ("-O", "write to a file"),
            ("-c", "continue a partial download"),
            ("-q", "quiet"),
            ("-r", "recursive"),
            ("--no-check-certificate", "skip TLS verification"),
        ],
    ),
    (
        "sed",
        &[
            ("-i", "edit files in place"),
            ("-E", "extended regex"),
            ("-n", "print only what is asked"),
            ("-e", "add a script"),
            ("-f", "read a script file"),
        ],
    ),
    (
        "awk",
        &[
            ("-F", "field separator"),
            ("-v", "assign a variable"),
            ("-f", "read a program file"),
        ],
    ),
    (
        "xargs",
        &[
            ("-n", "arguments per command"),
            ("-P", "run in parallel"),
            ("-I", "replace a placeholder"),
            ("-0", "null separated input"),
            ("-r", "skip when input is empty"),
            ("-t", "print each command"),
        ],
    ),
    (
        "sort",
        &[
            ("-n", "numeric"),
            ("-r", "reverse"),
            ("-k", "sort by field"),
            ("-t", "field separator"),
            ("-u", "drop duplicates"),
            ("-h", "human readable numbers"),
        ],
    ),
    (
        "uniq",
        &[
            ("-c", "count occurrences"),
            ("-d", "duplicates only"),
            ("-u", "unique lines only"),
            ("-i", "case insensitive"),
        ],
    ),
    ("head", &[("-n", "line count"), ("-c", "byte count")]),
    (
        "tail",
        &[
            ("-n", "line count"),
            ("-f", "follow as it grows"),
            ("-F", "follow across renames"),
            ("-c", "byte count"),
        ],
    ),
    (
        "wc",
        &[
            ("-l", "lines"),
            ("-w", "words"),
            ("-c", "bytes"),
            ("-m", "characters"),
        ],
    ),
    (
        "rsync",
        &[
            ("-a", "archive mode"),
            ("-v", "verbose"),
            ("-z", "compress in transit"),
            ("-P", "progress and resume"),
            ("-n", "dry run"),
            ("--delete", "remove what the source dropped"),
            ("--exclude", "skip matching paths"),
            ("-e", "remote shell to use"),
        ],
    ),
    (
        "journalctl",
        &[
            ("-u", "one unit"),
            ("-f", "follow"),
            ("-n", "last N lines"),
            ("-b", "this boot"),
            ("-p", "minimum priority"),
            ("--since", "from a time"),
            ("--until", "to a time"),
            ("--user", "user session log"),
            ("-k", "kernel messages"),
        ],
    ),
    (
        "systemctl",
        &[
            ("--user", "user manager"),
            ("--now", "start or stop as well"),
            ("--no-pager", "plain output"),
            ("-q", "quiet"),
            ("--failed", "only failed units"),
        ],
    ),
    (
        "ssh",
        &[
            ("-p", "port"),
            ("-i", "identity file"),
            ("-l", "login name"),
            ("-J", "jump host"),
            ("-A", "forward the agent"),
            ("-N", "no remote command"),
            ("-L", "local port forward"),
            ("-R", "remote port forward"),
            ("-o", "config option"),
            ("-v", "verbose"),
        ],
    ),
    (
        "scp",
        &[
            ("-r", "recursive"),
            ("-P", "port"),
            ("-i", "identity file"),
            ("-C", "compress"),
            ("-p", "preserve times and modes"),
        ],
    ),
    (
        "jq",
        &[
            ("-r", "raw output"),
            ("-c", "compact output"),
            ("-n", "no input"),
            ("-s", "slurp into an array"),
            ("-e", "exit status from the result"),
            ("--arg", "pass a string variable"),
        ],
    ),
    (
        "ln",
        &[
            ("-s", "symbolic link"),
            ("-f", "replace the target"),
            ("-n", "treat a link as a file"),
            ("-r", "relative symbolic link"),
        ],
    ),
    (
        "mv",
        &[
            ("-i", "ask before replacing"),
            ("-n", "never replace"),
            ("-v", "verbose"),
            ("-f", "replace without asking"),
        ],
    ),
    (
        "ping",
        &[
            ("-c", "stop after N packets"),
            ("-i", "seconds between packets"),
            ("-W", "reply timeout"),
            ("-4", "IPv4"),
            ("-6", "IPv6"),
        ],
    ),
    (
        "diff",
        &[
            ("-u", "unified context"),
            ("-r", "recursive"),
            ("-q", "report only whether they differ"),
            ("-w", "ignore whitespace"),
            ("--color", "colourise"),
        ],
    ),
];

/// ssh-family options whose next word is a value, not the destination.
const SSH_VALUE_OPTIONS: &[&str] = &[
    "-i", "-F", "-E", "-o", "-p", "-l", "-J", "-b", "-c", "-e", "-m", "-B", "-L", "-R", "-D", "-W",
    "-S", "-P",
];

const KILL_SIGNALS: &[(&str, &str)] = &[
    ("-1", "SIGHUP — hangup"),
    ("-2", "SIGINT — interrupt"),
    ("-3", "SIGQUIT — quit with core"),
    ("-9", "SIGKILL — force kill"),
    ("-15", "SIGTERM — terminate (default)"),
    ("-CONT", "resume a stopped process"),
    ("-HUP", "hangup"),
    ("-INT", "interrupt"),
    ("-KILL", "force kill"),
    ("-QUIT", "quit with core"),
    ("-STOP", "pause a process"),
    ("-TERM", "terminate (default)"),
    ("-USR1", "user-defined signal 1"),
    ("-USR2", "user-defined signal 2"),
];

/// Count destination-shaped words after an ssh-family command: everything
/// that is neither an option nor the value an option consumes.
fn ssh_positional_count(words: &[&str]) -> usize {
    let mut count = 0;
    let mut index = 1;
    while index < words.len() {
        let word = words[index];
        if word == "--" {
            count += words.len() - index - 1;
            break;
        }
        if word.starts_with('-') && word.len() > 1 {
            if SSH_VALUE_OPTIONS.contains(&word) {
                index += 1;
            }
        } else {
            count += 1;
        }
        index += 1;
    }
    count
}

fn complete_ssh_hosts(prefix: &str, colon_suffix: bool, home: &Path) -> Vec<Completion> {
    // `host:path` is already a remote path; nothing useful to offer.
    if prefix.contains(':') {
        return Vec::new();
    }
    let (user_prefix, host_prefix) = match prefix.rfind('@') {
        Some(pos) => (&prefix[..=pos], &prefix[pos + 1..]),
        None => ("", prefix),
    };

    let mut candidates: Vec<(String, &'static str)> = Vec::new();
    let mut seen_files = Vec::new();
    for host in ssh_config_hosts(&home.join(".ssh/config"), home, &mut seen_files, 0) {
        candidates.push((host, "ssh config"));
    }
    if let Ok(content) = crate::io_guard::read_regular_text(
        &home.join(".ssh/known_hosts"),
        MAX_COMPLETION_PROJECT_FILE_BYTES,
    ) {
        let mut known = parse_known_hosts(&content);
        known.sort();
        for host in known {
            candidates.push((host, "known host"));
        }
    }

    let mut seen = std::collections::HashSet::new();
    let hosts: Vec<Completion> = candidates
        .into_iter()
        .filter(|(host, _)| seen.insert(host.clone()))
        .take(MAX_COMPLETION_ITEMS)
        .map(|(host, source)| Completion::new(host, CompletionKind::Other).with_desc(source))
        .collect();

    // Rank on the bare host, then restore the user@ and scp colon spelling.
    rank_prefix_then_fuzzy(hosts, host_prefix)
        .into_iter()
        .map(|mut completion| {
            completion.text = if colon_suffix {
                format!("{user_prefix}{}:", completion.text)
            } else {
                format!("{user_prefix}{}", completion.text)
            };
            completion
        })
        .collect()
}

/// `Host` aliases from an ssh_config document. Patterns (`*`, `?`) and
/// negations match rather than name, so they are not completions.
fn parse_ssh_config_hosts(content: &str) -> Vec<String> {
    let (hosts, _) = parse_ssh_config_directives(content);
    hosts
}

/// `Host` aliases and the argument of every `Include`, in one pass.
fn parse_ssh_config_directives(content: &str) -> (Vec<String>, Vec<String>) {
    let mut hosts = Vec::new();
    let mut includes = Vec::new();
    for line in content.lines() {
        let line = line.trim();
        let Some((keyword, rest)) = line.split_once(|ch: char| ch.is_whitespace() || ch == '=')
        else {
            continue;
        };
        if keyword.eq_ignore_ascii_case("include") {
            includes.extend(rest.split_whitespace().map(str::to_string));
            continue;
        }
        // `Match host foo` describes when a block applies; the word after it
        // is a condition, not an alias someone can type.
        if !keyword.eq_ignore_ascii_case("host") {
            continue;
        }
        for pattern in rest.split_whitespace() {
            if pattern.contains('*') || pattern.contains('?') || pattern.starts_with('!') {
                continue;
            }
            hosts.push(pattern.to_string());
        }
    }
    (hosts, includes)
}

/// How far `Include` chains are followed, and how many files in total. Both
/// bound work done per keystroke; ssh's own limit is 16 levels.
const MAX_SSH_INCLUDE_DEPTH: usize = 8;
const MAX_SSH_CONFIG_FILES: usize = 64;

/// Host aliases from one ssh_config and everything it includes. Relative
/// include paths resolve against `~/.ssh`, as ssh resolves them for a user
/// config, and a file already read is not read again, so an include cycle
/// terminates.
fn ssh_config_hosts(
    path: &Path,
    home: &Path,
    seen: &mut Vec<PathBuf>,
    depth: usize,
) -> Vec<String> {
    if depth > MAX_SSH_INCLUDE_DEPTH || seen.len() >= MAX_SSH_CONFIG_FILES {
        return Vec::new();
    }
    let canonical = path.canonicalize().unwrap_or_else(|_| path.to_path_buf());
    if seen.contains(&canonical) {
        return Vec::new();
    }
    seen.push(canonical);

    let Ok(content) = crate::io_guard::read_regular_text(path, MAX_COMPLETION_PROJECT_FILE_BYTES)
    else {
        return Vec::new();
    };
    let (mut hosts, includes) = parse_ssh_config_directives(&content);
    for include in includes {
        for included in expand_ssh_include(&include, home) {
            hosts.extend(ssh_config_hosts(&included, home, seen, depth + 1));
        }
    }
    hosts
}

/// Resolve one `Include` argument to the files it names, expanding a trailing
/// glob against the directory it sits in. `~` and a bare relative path both
/// resolve the way ssh resolves them in a user config.
fn expand_ssh_include(pattern: &str, home: &Path) -> Vec<PathBuf> {
    let resolved = if let Some(rest) = pattern.strip_prefix("~/") {
        home.join(rest)
    } else if pattern.starts_with('/') {
        PathBuf::from(pattern)
    } else {
        home.join(".ssh").join(pattern)
    };

    let Some(name) = resolved.file_name().and_then(|name| name.to_str()) else {
        return Vec::new();
    };
    if !name.contains('*') && !name.contains('?') && !name.contains('[') {
        return vec![resolved];
    }
    let Some(parent) = resolved.parent() else {
        return Vec::new();
    };
    let Ok(entries) = fs::read_dir(parent) else {
        return Vec::new();
    };
    let mut matched: Vec<PathBuf> = entries
        .flatten()
        .filter(|entry| {
            entry
                .file_type()
                .map(|kind| kind.is_file())
                .unwrap_or(false)
                && entry
                    .file_name()
                    .to_str()
                    .is_some_and(|candidate| crate::glob_match::glob_match(name, candidate))
        })
        .map(|entry| entry.path())
        .take(MAX_SSH_CONFIG_FILES)
        .collect();
    matched.sort();
    matched
}

/// Plain host names from a known_hosts document. Hashed entries cannot be
/// completed, and raw IPv6 addresses need brackets ssh syntax elsewhere does
/// not use, so both are skipped.
fn parse_known_hosts(content: &str) -> Vec<String> {
    let mut hosts = Vec::new();
    for line in content.lines() {
        let line = line.trim();
        if line.is_empty() || line.starts_with('#') || line.starts_with('|') {
            continue;
        }
        let mut fields = line.split_whitespace();
        let mut field = fields.next().unwrap_or("");
        // `@cert-authority`/`@revoked` markers shift the host field right.
        if field.starts_with('@') {
            field = fields.next().unwrap_or("");
        }
        for host in field.split(',') {
            let host = host
                .strip_prefix('[')
                .and_then(|h| h.split_once("]:"))
                .map(|(h, _)| h)
                .unwrap_or(host);
            if host.is_empty() || host.contains(':') || host.contains('*') || host.contains('?') {
                continue;
            }
            hosts.push(host.to_string());
        }
    }
    hosts
}

/// Bare variable names for `export`/`unset`/`declare`-style builtins.
fn complete_variable_names(prefix: &str, state: &ShellState) -> Vec<Completion> {
    let mut entries: Vec<(String, &'static str)> = Vec::new();
    for name in state.env_vars.keys() {
        entries.push((name.clone(), "environment variable"));
    }
    for scope in &state.local_vars_stack {
        for name in scope.keys() {
            entries.push((name.clone(), "local"));
        }
    }
    for name in state.arrays.keys() {
        entries.push((name.clone(), "array"));
    }
    for name in state.assoc_arrays.keys() {
        entries.push((name.clone(), "assoc array"));
    }
    entries.sort_by(|a, b| a.0.cmp(&b.0));
    let mut seen = std::collections::HashSet::new();
    entries
        .into_iter()
        .filter(|(name, _)| name.starts_with(prefix) && seen.insert(name.clone()))
        .map(|(name, desc)| Completion::new(name, CompletionKind::Variable).with_desc(desc))
        .collect()
}

fn complete_job_specs(prefix: &str, state: &ShellState) -> Vec<Completion> {
    state
        .jobs
        .jobs
        .iter()
        .filter_map(|job| {
            let spec = format!("%{}", job.id);
            if !spec.starts_with(prefix) {
                return None;
            }
            Some(
                Completion::new(spec, CompletionKind::Other)
                    .with_desc(&format!("{} — {}", job.status, job.command)),
            )
        })
        .collect()
}

/// `${PREFIX` completion: same candidates as `$PREFIX`, spelled with braces
/// and closed so the result is immediately valid.
fn complete_variable_braced(prefix: &str, state: &ShellState) -> Vec<Completion> {
    complete_variable(prefix, state)
        .into_iter()
        .map(|mut completion| {
            if let Some(name) = completion.text.strip_prefix('$') {
                if !name.starts_with('{') {
                    completion.text = format!("${{{name}}}");
                }
            }
            completion
        })
        .collect()
}

fn z_frecency_completions(prefix: &str) -> Vec<Completion> {
    let Ok(db) = crate::zjump::get_z_db().lock() else {
        return Vec::new();
    };
    let entries = db.list();
    z_entry_completions(&entries, prefix)
}

/// Highest-frecency directories whose path contains the typed prefix, the
/// same matching `z` itself applies.
fn z_entry_completions(entries: &[(String, f64)], prefix: &str) -> Vec<Completion> {
    let needle = unescape_shell_word(prefix).to_lowercase();
    entries
        .iter()
        .filter(|(path, _)| needle.is_empty() || path.to_lowercase().contains(&needle))
        .take(8)
        .map(|(path, _)| Completion {
            text: escape_shell_word(path),
            display: path.clone(),
            description: Some("frecent directory".to_string()),
            kind: CompletionKind::Directory,
            is_dir: false,
        })
        .collect()
}

const MAX_DOCKER_COMPLETION_BYTES: usize = 1024 * 1024;
const HELPER_PROBE_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(2);
const MAX_SYSTEM_PID_RESULTS: usize = 30;

/// Run a trusted helper with bounded output and time, for completion probes.
fn bounded_helper_stdout(helper: &str, args: &[&str], max_bytes: usize) -> Option<String> {
    probe_text_once(&format!("{helper}:{}", args.join(" ")), || {
        let path = crate::io_guard::trusted_helper(helper)?;
        let mut command = std::process::Command::new(path);
        command.args(args);
        let output = crate::io_guard::bounded_command_output(
            &mut command,
            max_bytes,
            64 * 1024,
            HELPER_PROBE_TIMEOUT,
        )
        .ok()?;
        output
            .status
            .success()
            .then(|| String::from_utf8_lossy(&output.stdout).into_owned())
    })
}

fn bounded_docker_stdout(args: &[&str]) -> Option<String> {
    bounded_helper_stdout("docker", args, MAX_DOCKER_COMPLETION_BYTES)
}

fn complete_docker_containers(prefix: &str, include_stopped: bool) -> Vec<Completion> {
    let format = "{{.Names}}\t{{.Image}}\t{{.Status}}";
    let args: &[&str] = if include_stopped {
        &["ps", "-a", "--format", format]
    } else {
        &["ps", "--format", format]
    };
    let Some(output) = bounded_docker_stdout(args) else {
        return Vec::new();
    };
    parse_docker_containers(&output, prefix)
}

fn parse_docker_containers(output: &str, prefix: &str) -> Vec<Completion> {
    output
        .lines()
        .filter_map(|line| {
            let mut parts = line.split('\t');
            let name = parts.next()?.trim();
            if name.is_empty() || !name.starts_with(prefix) {
                return None;
            }
            let image = parts.next().unwrap_or("").trim();
            let status = parts.next().unwrap_or("").trim();
            let description = match (image.is_empty(), status.is_empty()) {
                (false, false) => format!("{image} — {status}"),
                (false, true) => image.to_string(),
                _ => status.to_string(),
            };
            Some(Completion::new(name.to_string(), CompletionKind::Other).with_desc(&description))
        })
        .take(MAX_COMPLETION_ITEMS)
        .collect()
}

fn complete_docker_images(prefix: &str) -> Vec<Completion> {
    let Some(output) =
        bounded_docker_stdout(&["images", "--format", "{{.Repository}}:{{.Tag}}\t{{.Size}}"])
    else {
        return Vec::new();
    };
    parse_docker_images(&output, prefix)
}

fn parse_docker_images(output: &str, prefix: &str) -> Vec<Completion> {
    let mut seen = std::collections::HashSet::new();
    output
        .lines()
        .filter_map(|line| {
            let mut parts = line.split('\t');
            let mut reference = parts.next()?.trim();
            // An untagged image shows as `repo:<none>`; the repository alone
            // is still a valid argument. Fully dangling images are skipped.
            if reference.contains("<none>") {
                reference = reference.strip_suffix(":<none>")?;
                if reference.contains("<none>") {
                    return None;
                }
            }
            if !reference.starts_with(prefix) || !seen.insert(reference.to_string()) {
                return None;
            }
            let size = parts.next().unwrap_or("").trim();
            let completion = Completion::new(reference.to_string(), CompletionKind::Other);
            Some(if size.is_empty() {
                completion
            } else {
                completion.with_desc(size)
            })
        })
        .take(MAX_COMPLETION_ITEMS)
        .collect()
}

/// The current user's own processes from /proc, newest first, for `kill`.
/// The shell itself is excluded; killing it from its own completion list is
/// never what was meant.
fn complete_system_pids(prefix: &str) -> Vec<Completion> {
    let own_uid = nix::unistd::geteuid().as_raw();
    let own_pid = std::process::id();
    let Ok(entries) = fs::read_dir("/proc") else {
        return Vec::new();
    };
    let mut processes: Vec<(u32, String)> = entries
        .flatten()
        .filter_map(|entry| {
            let pid: u32 = entry.file_name().to_str()?.parse().ok()?;
            if pid == own_pid {
                return None;
            }
            use std::os::unix::fs::MetadataExt;
            if entry.metadata().ok()?.uid() != own_uid {
                return None;
            }
            let comm = crate::io_guard::read_regular_text(&entry.path().join("comm"), 256).ok()?;
            Some((pid, comm.trim().to_string()))
        })
        .collect();
    processes.sort_by_key(|process| std::cmp::Reverse(process.0));
    processes
        .into_iter()
        .filter(|(pid, _)| pid.to_string().starts_with(prefix))
        .take(MAX_SYSTEM_PID_RESULTS)
        .map(|(pid, comm)| Completion::new(pid.to_string(), CompletionKind::Other).with_desc(&comm))
        .collect()
}

const MAX_SYSTEMCTL_COMPLETION_BYTES: usize = 4 * 1024 * 1024;
const MAX_USER_DB_BYTES: usize = 4 * 1024 * 1024;

/// Shell-condition names for `trap`, plus the signals worth trapping.
const TRAP_SIGNALS: &[(&str, &str)] = &[
    ("EXIT", "when the shell exits"),
    ("ERR", "when a command fails"),
    ("DEBUG", "before every command"),
    ("HUP", "hangup"),
    ("INT", "on Ctrl-C"),
    ("QUIT", "quit with core"),
    ("ABRT", "abort"),
    ("ALRM", "timer expired"),
    ("TERM", "terminate"),
    ("USR1", "user-defined signal 1"),
    ("USR2", "user-defined signal 2"),
    ("PIPE", "broken pipe"),
    ("CHLD", "child state changed"),
    ("CONT", "continued after stop"),
    ("WINCH", "terminal resized"),
];

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum WrapperValueKind {
    User,
    Group,
}

/// Is the word being completed the value of a wrapper option that names a
/// user or group (`sudo -u <TAB>`)? Only while still inside sudo's own option
/// zone — once the wrapped command starts, its flags are its own.
fn wrapper_value_kind(before: &str) -> Option<WrapperValueKind> {
    let segment = active_command_segment(before);
    let words: Vec<&str> = segment.split_whitespace().collect();
    let mut index = 0;
    while index < words.len() && is_assignment_word(words[index]) {
        index += 1;
    }
    if words.get(index).copied() != Some("sudo") {
        return None;
    }
    index += 1;
    const OTHER_VALUE_OPTIONS: &[&str] = &[
        "-p",
        "--prompt",
        "-h",
        "--host",
        "-C",
        "--close-from",
        "-T",
        "--command-timeout",
        "-R",
        "--chroot",
        "-D",
        "--chdir",
    ];
    while index < words.len() {
        let word = words[index];
        let kind = match word {
            "-u" | "--user" => Some(WrapperValueKind::User),
            "-g" | "--group" => Some(WrapperValueKind::Group),
            _ => None,
        };
        if let Some(kind) = kind {
            if index + 1 == words.len() {
                return Some(kind);
            }
            index += 2;
            continue;
        }
        if !word.starts_with('-') {
            return None;
        }
        index += 1;
        if OTHER_VALUE_OPTIONS.contains(&word) {
            index += 1;
        }
    }
    None
}

fn complete_systemctl_units(prefix: &str, user_scope: bool, unit_files: bool) -> Vec<Completion> {
    let mut args: Vec<&str> = if unit_files {
        vec!["list-unit-files", "--no-legend", "--plain", "--full"]
    } else {
        vec!["list-units", "--all", "--no-legend", "--plain", "--full"]
    };
    if user_scope {
        args.push("--user");
    }
    let Some(output) = bounded_helper_stdout("systemctl", &args, MAX_SYSTEMCTL_COMPLETION_BYTES)
    else {
        return Vec::new();
    };
    if unit_files {
        parse_systemctl_unit_files(&output, prefix)
    } else {
        parse_systemctl_units(&output, prefix)
    }
}

/// `list-units --no-legend --plain` lines: UNIT LOAD ACTIVE SUB DESCRIPTION.
fn parse_systemctl_units(output: &str, prefix: &str) -> Vec<Completion> {
    output
        .lines()
        .filter_map(|line| {
            let mut parts = line.split_whitespace();
            let unit = parts.next()?;
            if !unit.starts_with(prefix) || unit.contains("@.") {
                return None;
            }
            let _load = parts.next();
            let active = parts.next().unwrap_or("");
            let _sub = parts.next();
            let description = parts.collect::<Vec<_>>().join(" ");
            let label = if description.is_empty() {
                active.to_string()
            } else {
                format!("{active} — {description}")
            };
            Some(Completion::new(unit.to_string(), CompletionKind::Other).with_desc(&label))
        })
        .take(MAX_COMPLETION_ITEMS)
        .collect()
}

/// `list-unit-files --no-legend --plain` lines: UNIT FILE, STATE, PRESET.
/// Template units (`getty@.service`) need an instance and cannot be operated
/// on by their file name alone.
fn parse_systemctl_unit_files(output: &str, prefix: &str) -> Vec<Completion> {
    output
        .lines()
        .filter_map(|line| {
            let mut parts = line.split_whitespace();
            let unit = parts.next()?;
            if !unit.starts_with(prefix) || unit.contains("@.") {
                return None;
            }
            let state = parts.next().unwrap_or("");
            Some(Completion::new(unit.to_string(), CompletionKind::Other).with_desc(state))
        })
        .take(MAX_COMPLETION_ITEMS)
        .collect()
}

fn complete_users(prefix: &str) -> Vec<Completion> {
    let Ok(content) =
        crate::io_guard::read_regular_text(Path::new("/etc/passwd"), MAX_USER_DB_BYTES)
    else {
        return Vec::new();
    };
    parse_passwd_users(&content, prefix)
}

/// Users from a passwd document, human accounts (uid 0 and >= 1000, short of
/// nobody) ahead of system accounts.
fn parse_passwd_users(content: &str, prefix: &str) -> Vec<Completion> {
    let mut human = Vec::new();
    let mut system = Vec::new();
    for line in content.lines() {
        let mut fields = line.split(':');
        let Some(name) = fields.next() else { continue };
        if name.is_empty() || !name.starts_with(prefix) {
            continue;
        }
        let Some(uid) = fields.nth(1).and_then(|uid| uid.parse::<u32>().ok()) else {
            continue;
        };
        let completion = Completion::new(name.to_string(), CompletionKind::Other)
            .with_desc(&format!("uid {uid}"));
        if uid == 0 || (uid >= 1000 && uid != 65534) {
            human.push(completion);
        } else {
            system.push(completion);
        }
    }
    human.sort_by(|a, b| a.text.cmp(&b.text));
    system.sort_by(|a, b| a.text.cmp(&b.text));
    human.extend(system);
    human
}

fn complete_groups(prefix: &str) -> Vec<Completion> {
    let Ok(content) =
        crate::io_guard::read_regular_text(Path::new("/etc/group"), MAX_USER_DB_BYTES)
    else {
        return Vec::new();
    };
    parse_group_entries(&content, prefix)
}

fn parse_group_entries(content: &str, prefix: &str) -> Vec<Completion> {
    let mut human = Vec::new();
    let mut system = Vec::new();
    for line in content.lines() {
        let mut fields = line.split(':');
        let Some(name) = fields.next() else { continue };
        if name.is_empty() || !name.starts_with(prefix) {
            continue;
        }
        let Some(gid) = fields.nth(1).and_then(|gid| gid.parse::<u32>().ok()) else {
            continue;
        };
        let completion = Completion::new(name.to_string(), CompletionKind::Other)
            .with_desc(&format!("gid {gid}"));
        if gid == 0 || (gid >= 1000 && gid != 65534) {
            human.push(completion);
        } else {
            system.push(completion);
        }
    }
    human.sort_by(|a, b| a.text.cmp(&b.text));
    system.sort_by(|a, b| a.text.cmp(&b.text));
    human.extend(system);
    human
}

/// Contexts, clusters, users and namespaces from the local kubeconfig.
///
/// Deliberately file-only: a resource name would have to come from the API
/// server, and a Tab keystroke must never become a network round trip to a
/// cluster that may be unreachable, slow, or somewhere else entirely. What
/// the file holds is exactly what `kubectl config` edits.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum KubeName {
    Context,
    Cluster,
    User,
    Namespace,
}

/// `KUBECONFIG` is a path list, like PATH, and wins over the default. It is
/// read from this shell's own environment rather than the process's: an
/// `export` typed at this prompt is what the next command will see.
fn kubeconfig_paths(state: &ShellState) -> Vec<PathBuf> {
    if let Some(value) = state.env_vars.get("KUBECONFIG") {
        let paths: Vec<PathBuf> = value
            .split(':')
            .filter(|entry| !entry.is_empty())
            .map(PathBuf::from)
            .take(MAX_SSH_CONFIG_FILES)
            .collect();
        if !paths.is_empty() {
            return paths;
        }
    }
    vec![state.home_dir.join(".kube/config")]
}

fn complete_kube_names(prefix: &str, kind: KubeName, state: &ShellState) -> Vec<Completion> {
    let mut completions = Vec::new();
    let mut seen = std::collections::HashSet::new();
    for path in kubeconfig_paths(state) {
        let Ok(content) =
            crate::io_guard::read_regular_text(&path, MAX_COMPLETION_PROJECT_FILE_BYTES)
        else {
            continue;
        };
        for completion in parse_kubeconfig_names(&content, kind, prefix) {
            if seen.insert(completion.text.clone()) {
                completions.push(completion);
            }
        }
    }
    completions
}

fn parse_kubeconfig_names(content: &str, kind: KubeName, prefix: &str) -> Vec<Completion> {
    let Ok(document) = serde_yaml::from_str::<serde_yaml::Value>(content) else {
        return Vec::new();
    };
    let current = document
        .get("current-context")
        .and_then(|value| value.as_str());

    // Namespaces are not a top-level list: each context names the one it
    // defaults to, and those are the namespaces this machine knows about.
    if kind == KubeName::Namespace {
        let Some(contexts) = document
            .get("contexts")
            .and_then(|value| value.as_sequence())
        else {
            return Vec::new();
        };
        let mut seen = std::collections::HashSet::new();
        return contexts
            .iter()
            .filter_map(|entry| {
                let namespace = entry.get("context")?.get("namespace")?.as_str()?;
                if !namespace.starts_with(prefix) || !seen.insert(namespace.to_string()) {
                    return None;
                }
                let context = entry.get("name").and_then(|value| value.as_str());
                Some(
                    Completion::new(namespace.to_string(), CompletionKind::Other).with_desc(
                        &match context {
                            Some(context) => format!("default for {context}"),
                            None => "kubeconfig namespace".to_string(),
                        },
                    ),
                )
            })
            .take(MAX_COMPLETION_ITEMS)
            .collect();
    }

    let (section, label) = match kind {
        KubeName::Context => ("contexts", "context"),
        KubeName::Cluster => ("clusters", "cluster"),
        KubeName::User => ("users", "user"),
        KubeName::Namespace => unreachable!(),
    };
    let Some(entries) = document.get(section).and_then(|value| value.as_sequence()) else {
        return Vec::new();
    };
    entries
        .iter()
        .filter_map(|entry| {
            let name = entry.get("name")?.as_str()?;
            if !name.starts_with(prefix) {
                return None;
            }
            let description = if Some(name) == current {
                "current context".to_string()
            } else {
                label.to_string()
            };
            Some(Completion::new(name.to_string(), CompletionKind::Other).with_desc(&description))
        })
        .take(MAX_COMPLETION_ITEMS)
        .collect()
}

/// The compose file names Docker itself looks for, in its own order.
const COMPOSE_FILE_NAMES: &[&str] = &[
    "compose.yaml",
    "compose.yml",
    "docker-compose.yaml",
    "docker-compose.yml",
];

/// Service names from the nearest compose file. This reads the project's own
/// file rather than asking the daemon: the services a person is about to
/// `up` are the ones written down, not only the ones already running.
fn complete_compose_services(prefix: &str) -> Vec<Completion> {
    let Some(path) = COMPOSE_FILE_NAMES
        .iter()
        .find_map(|name| find_upwards(name))
    else {
        return Vec::new();
    };
    compose_services_from_path(&path, prefix)
}

fn compose_services_from_path(path: &Path, prefix: &str) -> Vec<Completion> {
    let Ok(content) = crate::io_guard::read_regular_text(path, MAX_COMPLETION_PROJECT_FILE_BYTES)
    else {
        return Vec::new();
    };
    let Ok(document) = serde_yaml::from_str::<serde_yaml::Value>(&content) else {
        return Vec::new();
    };
    let Some(services) = document
        .get("services")
        .and_then(|value| value.as_mapping())
    else {
        return Vec::new();
    };
    services
        .iter()
        .filter_map(|(name, definition)| {
            let name = name.as_str()?;
            if !name.starts_with(prefix) {
                return None;
            }
            // The image or the build context says what the service is.
            let description = definition
                .get("image")
                .and_then(|value| value.as_str())
                .map(str::to_string)
                .or_else(|| {
                    let build = definition.get("build")?;
                    build
                        .as_str()
                        .map(|context| format!("build {context}"))
                        .or_else(|| {
                            let context = build.get("context")?.as_str()?;
                            Some(format!("build {context}"))
                        })
                })
                .unwrap_or_else(|| "compose service".to_string());
            Some(Completion::new(name.to_string(), CompletionKind::Other).with_desc(&description))
        })
        .take(MAX_COMPLETION_ITEMS)
        .collect()
}

/// Dependencies from the nearest package.json, for uninstall/update-style
/// package-manager subcommands.
fn complete_npm_dependencies(prefix: &str) -> Vec<Completion> {
    let Some(path) = find_upwards("package.json") else {
        return Vec::new();
    };
    npm_dependencies_from_path(&path, prefix)
}

fn npm_dependencies_from_path(path: &std::path::Path, prefix: &str) -> Vec<Completion> {
    let Ok(content) = crate::io_guard::read_regular_text(path, MAX_COMPLETION_PROJECT_FILE_BYTES)
    else {
        return Vec::new();
    };
    let Ok(json) = serde_json::from_str::<serde_json::Value>(&content) else {
        return Vec::new();
    };
    let mut results = Vec::new();
    let mut seen = std::collections::HashSet::new();
    for (section, label) in [
        ("dependencies", "dependency"),
        ("devDependencies", "dev dependency"),
    ] {
        let Some(deps) = json.get(section).and_then(serde_json::Value::as_object) else {
            continue;
        };
        for (name, version) in deps {
            if !name.starts_with(prefix) || !seen.insert(name.clone()) {
                continue;
            }
            let description = match version.as_str() {
                Some(version) => format!("{label} {version}"),
                None => label.to_string(),
            };
            results
                .push(Completion::new(name.clone(), CompletionKind::Other).with_desc(&description));
        }
    }
    results
}

fn promote_git_context(
    completions: &mut Vec<Completion>,
    value: &str,
    prefix: &str,
    description: &str,
) {
    if !value.starts_with(prefix) {
        return;
    }
    if let Some(index) = completions
        .iter()
        .position(|completion| completion.text == value)
    {
        let mut completion = completions.remove(index);
        completion.description = Some(description.to_string());
        completions.insert(0, completion);
    } else {
        completions.insert(
            0,
            Completion {
                text: value.to_string(),
                display: value.to_string(),
                description: Some(description.to_string()),
                kind: CompletionKind::Other,
                is_dir: false,
            },
        );
    }
}

fn complete_git_refs(prefix: &str) -> Vec<Completion> {
    if let Some(output) = probe_text_once("git:refs", || {
        crate::prompt::bounded_git_stdout(
            Path::new("."),
            &[
                "for-each-ref",
                "--format=%(refname)",
                "refs/heads",
                "refs/remotes",
                "refs/tags",
            ],
            MAX_GIT_COMPLETION_BYTES,
        )
        .map(|output| String::from_utf8_lossy(&output).into_owned())
    }) {
        return parse_git_refs(&output, prefix);
    }
    Vec::new()
}

/// Local branches only, most recently committed first, for the arguments
/// that mean a branch rather than any ref.
fn complete_git_local_branches(prefix: &str) -> Vec<Completion> {
    let Some(output) = probe_text_once("git:local-branches", || {
        crate::prompt::bounded_git_stdout(
            Path::new("."),
            &[
                "for-each-ref",
                "--sort=-committerdate",
                "--format=%(refname:short)%09%(contents:subject)",
                "refs/heads",
            ],
            MAX_GIT_COMPLETION_BYTES,
        )
        .map(|output| String::from_utf8_lossy(&output).into_owned())
    }) else {
        return Vec::new();
    };
    output
        .lines()
        .filter_map(|line| {
            let (branch, subject) = line.split_once('\t').unwrap_or((line, ""));
            let branch = branch.trim();
            if branch.is_empty() || !branch.starts_with(prefix) {
                return None;
            }
            let completion = Completion::new(branch.to_string(), CompletionKind::Other);
            Some(if subject.is_empty() {
                completion.with_desc("branch")
            } else {
                completion.with_desc(subject)
            })
        })
        .take(MAX_COMPLETION_ITEMS)
        .collect()
}

fn complete_git_tags(prefix: &str) -> Vec<Completion> {
    let Some(output) = probe_text_once("git:tags", || {
        crate::prompt::bounded_git_stdout(
            Path::new("."),
            &[
                "for-each-ref",
                "--sort=-creatordate",
                "--format=%(refname:short)",
                "refs/tags",
            ],
            MAX_GIT_COMPLETION_BYTES,
        )
        .map(|output| String::from_utf8_lossy(&output).into_owned())
    }) else {
        return Vec::new();
    };
    output
        .lines()
        .map(str::trim)
        .filter(|tag| !tag.is_empty() && tag.starts_with(prefix))
        .map(|tag| Completion::new(tag.to_string(), CompletionKind::Other).with_desc("tag"))
        .take(MAX_COMPLETION_ITEMS)
        .collect()
}

/// Worktree paths as `git worktree list --porcelain` reports them.
fn complete_git_worktrees(prefix: &str) -> Vec<Completion> {
    let Some(output) = probe_text_once("git:worktrees", || {
        crate::prompt::bounded_git_stdout(
            Path::new("."),
            &["worktree", "list", "--porcelain"],
            MAX_GIT_COMPLETION_BYTES,
        )
        .map(|output| String::from_utf8_lossy(&output).into_owned())
    }) else {
        return Vec::new();
    };
    parse_git_worktrees(&output, prefix)
}

fn parse_git_worktrees(output: &str, prefix: &str) -> Vec<Completion> {
    let mut completions = Vec::new();
    let mut path: Option<&str> = None;
    for line in output.lines() {
        if let Some(value) = line.strip_prefix("worktree ") {
            path = Some(value.trim());
            continue;
        }
        let Some(current) = path else { continue };
        // The line after the path says which branch is checked out there.
        let description = if let Some(branch) = line.strip_prefix("branch ") {
            branch.trim().trim_start_matches("refs/heads/").to_string()
        } else if line.starts_with("detached") {
            "detached HEAD".to_string()
        } else {
            continue;
        };
        path = None;
        if !current.starts_with(prefix) {
            continue;
        }
        completions.push(
            Completion::new(escape_shell_word(current), CompletionKind::Directory)
                .with_desc(&description),
        );
    }
    completions.truncate(MAX_COMPLETION_ITEMS);
    completions
}

/// Keys already set in any scope, with their values, so `git config --get`
/// completes what this repository actually has.
fn complete_git_config_keys(prefix: &str) -> Vec<Completion> {
    let Some(output) = probe_text_once("git:config-keys", || {
        crate::prompt::bounded_git_stdout(
            Path::new("."),
            &["config", "--list", "--name-only"],
            MAX_GIT_COMPLETION_BYTES,
        )
        .map(|output| String::from_utf8_lossy(&output).into_owned())
    }) else {
        return Vec::new();
    };
    let mut seen = std::collections::HashSet::new();
    output
        .lines()
        .map(str::trim)
        .filter(|key| !key.is_empty() && key.starts_with(prefix))
        .filter(|key| seen.insert(key.to_string()))
        .map(|key| Completion::new(key.to_string(), CompletionKind::Other).with_desc("set here"))
        .take(MAX_COMPLETION_ITEMS)
        .collect()
}

/// Keys worth offering even when nothing has set them yet.
const GIT_CONFIG_KEYS: &[(&str, &str)] = &[
    ("user.name", "Name recorded on commits"),
    ("user.email", "Email recorded on commits"),
    ("core.editor", "Editor for messages"),
    ("core.excludesfile", "Global ignore file"),
    ("init.defaultBranch", "Branch name for new repositories"),
    ("pull.rebase", "Rebase instead of merge on pull"),
    ("push.default", "What a bare push pushes"),
    ("push.autoSetupRemote", "Create the upstream on first push"),
    ("merge.conflictstyle", "Conflict marker style"),
    ("rebase.autostash", "Stash before rebasing"),
    ("diff.tool", "External diff tool"),
    ("commit.gpgsign", "Sign commits"),
    ("fetch.prune", "Drop deleted remote branches on fetch"),
    ("alias.", "Define a Git alias"),
];

fn parse_git_refs(output: &str, prefix: &str) -> Vec<Completion> {
    let mut completions = Vec::new();
    for reference in output.lines().map(str::trim) {
        if completions.len() >= MAX_COMPLETION_ITEMS {
            break;
        }
        if let Some(branch) = reference.strip_prefix("refs/heads/") {
            if branch.starts_with(prefix) {
                completions.push(Completion {
                    text: branch.to_string(),
                    display: branch.to_string(),
                    description: Some("branch".to_string()),
                    kind: CompletionKind::Other,
                    is_dir: false,
                });
            }
        } else if let Some(tag) = reference.strip_prefix("refs/tags/") {
            if tag.starts_with(prefix) && !completions.iter().any(|item| item.text == tag) {
                completions.push(Completion {
                    text: tag.to_string(),
                    display: tag.to_string(),
                    description: Some("tag".to_string()),
                    kind: CompletionKind::Other,
                    is_dir: false,
                });
            }
        } else if let Some(branch) = reference.strip_prefix("refs/remotes/") {
            let Some((remote, short)) = split_remote_branch(branch) else {
                continue;
            };
            if short.starts_with(prefix) && !completions.iter().any(|item| item.text == short) {
                completions.push(Completion {
                    text: short.to_string(),
                    display: short.to_string(),
                    description: Some(format!("remote ({})", remote)),
                    kind: CompletionKind::Other,
                    is_dir: false,
                });
            }
        }
    }
    completions
}

fn git_file_description(status: [u8; 2], context: &str) -> Option<&'static str> {
    let [index, worktree] = status;
    match context {
        "add" => {
            if status == [b'?', b'?'] {
                return Some("untracked");
            }
            match worktree {
                b'M' => Some("modified"),
                b'D' => Some("deleted"),
                b'R' => Some("renamed"),
                b'U' => Some("unmerged"),
                b'T' => Some("type changed"),
                b' ' => None,
                _ => Some("changed"),
            }
        }
        "restore" => match worktree {
            b'M' => Some("modified"),
            b'D' => Some("deleted"),
            b'R' => Some("renamed"),
            b'U' => Some("unmerged"),
            b'T' => Some("type changed"),
            _ => None,
        },
        "restore_staged" | "reset" => match index {
            b'M' | b'A' | b'D' | b'R' | b'C' | b'T' | b'U' => Some("staged"),
            _ => None,
        },
        _ => None,
    }
}

fn split_remote_branch(branch: &str) -> Option<(&str, &str)> {
    if branch.ends_with("/HEAD") {
        return None;
    }
    let (remote, short) = branch.split_once('/')?;
    (!remote.is_empty() && !short.is_empty()).then_some((remote, short))
}

fn complete_git_dirty_files(prefix: &str, context: &str) -> Vec<Completion> {
    let mut completions = Vec::new();
    if let Some(output) = probe_once("git:status-z", || {
        crate::prompt::bounded_git_stdout(
            Path::new("."),
            &["status", "--porcelain=v1", "-z"],
            MAX_GIT_COMPLETION_BYTES,
        )
    }) {
        let decoded_prefix = unescape_shell_word(prefix);
        for (status, file) in parse_git_status_entries(&output) {
            if completions.len() >= MAX_COMPLETION_ITEMS {
                break;
            }
            if !file.starts_with(&decoded_prefix) {
                continue;
            }
            if let Some(desc) = git_file_description(status, context) {
                completions.push(Completion {
                    text: escape_shell_word(&file),
                    display: file,
                    description: Some(desc.to_string()),
                    kind: CompletionKind::File,
                    is_dir: false,
                });
            }
        }
    }
    completions
}

/// Parse `git status --porcelain=v1 -z`. Rename/copy records contain a second
/// NUL-delimited source path; completion should insert the destination path.
fn parse_git_status_entries(output: &[u8]) -> Vec<([u8; 2], String)> {
    let mut fields = output
        .split(|byte| *byte == 0)
        .filter(|field| !field.is_empty());
    let mut entries = Vec::new();
    while let Some(field) = fields.next() {
        if entries.len() >= MAX_COMPLETION_ITEMS {
            break;
        }
        if field.len() < 4 || field[2] != b' ' {
            continue;
        }
        let status = [field[0], field[1]];
        entries.push((status, String::from_utf8_lossy(&field[3..]).into_owned()));
        if status.iter().any(|code| matches!(code, b'R' | b'C')) {
            let _ = fields.next();
        }
    }
    entries
}

fn complete_git_stashes(prefix: &str) -> Vec<Completion> {
    let mut completions = Vec::new();
    if let Some(output) = crate::prompt::bounded_git_stdout(
        Path::new("."),
        &["stash", "list", "--format=%gd|%gs"],
        MAX_GIT_COMPLETION_BYTES,
    ) {
        let stdout = String::from_utf8_lossy(&output);
        for line in stdout.lines() {
            if completions.len() >= MAX_COMPLETION_ITEMS {
                break;
            }
            let parts: Vec<&str> = line.splitn(2, '|').collect();
            let (ref_name, msg) = if parts.len() == 2 {
                (parts[0], parts[1])
            } else {
                (line, "")
            };
            if ref_name.starts_with(prefix) || prefix.is_empty() {
                completions.push(Completion {
                    text: ref_name.to_string(),
                    display: ref_name.to_string(),
                    description: Some(msg.to_string()),
                    kind: CompletionKind::Other,
                    is_dir: false,
                });
            }
        }
    }
    completions
}

fn complete_git_recent_commits(prefix: &str) -> Vec<Completion> {
    let mut completions = Vec::new();
    if let Some(output) = crate::prompt::bounded_git_stdout(
        Path::new("."),
        &["log", "--oneline", "-20", "--format=%h|%s"],
        MAX_GIT_COMPLETION_BYTES,
    ) {
        let stdout = String::from_utf8_lossy(&output);
        for line in stdout.lines() {
            let parts: Vec<&str> = line.splitn(2, '|').collect();
            let (hash, msg) = if parts.len() == 2 {
                (parts[0], parts[1])
            } else {
                (line, "")
            };
            if hash.starts_with(prefix) || prefix.is_empty() {
                let desc = if msg.len() > 40 {
                    format!("{}…", msg.chars().take(39).collect::<String>())
                } else {
                    msg.to_string()
                };
                completions.push(Completion {
                    text: hash.to_string(),
                    display: hash.to_string(),
                    description: Some(desc),
                    kind: CompletionKind::Other,
                    is_dir: false,
                });
            }
        }
    }
    completions
}

fn complete_git_remotes(prefix: &str) -> Vec<Completion> {
    let mut completions = Vec::new();
    if let Some(stdout) = probe_text_once("git:remotes", || {
        crate::prompt::bounded_git_stdout(Path::new("."), &["remote"], MAX_GIT_COMPLETION_BYTES)
            .map(|output| String::from_utf8_lossy(&output).into_owned())
    }) {
        for remote in stdout.lines() {
            if completions.len() >= MAX_COMPLETION_ITEMS {
                break;
            }
            let remote = remote.trim();
            if !remote.is_empty() && remote.starts_with(prefix) {
                completions.push(Completion {
                    text: remote.to_string(),
                    display: remote.to_string(),
                    description: Some("remote".to_string()),
                    kind: CompletionKind::Other,
                    is_dir: false,
                });
            }
        }
    }
    completions
}

fn complete_from_spec(
    cmd: &str,
    prefix: &str,
    segment: &str,
    state: &ShellState,
) -> Option<Vec<Completion>> {
    use crate::completion_spec::SpecCompletionKind;

    let words: Vec<&str> = command_words(segment).collect();
    let ctx = state.spec_registry.resolve_context(cmd, &words)?;

    if let Some((option_name, value_prefix)) = prefix.split_once('=') {
        if let Some(option) = ctx
            .options
            .iter()
            .find(|option| option.names.iter().any(|name| name == option_name))
        {
            let results = complete_spec_args(&option.args, value_prefix, state)
                .into_iter()
                .map(|mut completion| {
                    completion.text = format!("{}={}", option_name, completion.text);
                    completion
                })
                .collect::<Vec<_>>();
            if !results.is_empty() {
                return Some(results);
            }
        }
    }

    let current_is_empty = segment.chars().last().is_some_and(char::is_whitespace);
    let previous = if current_is_empty {
        words.last().copied()
    } else {
        words.get(words.len().saturating_sub(2)).copied()
    };
    if let Some(option_name) = previous.filter(|word| word.starts_with('-')) {
        if let Some(option) = ctx
            .options
            .iter()
            .find(|option| option.names.iter().any(|name| name == option_name))
        {
            let results = complete_spec_args(&option.args, prefix, state);
            if !results.is_empty() {
                return Some(results);
            }
        }
    }

    let results = ctx.complete_prefix(prefix);
    if results.is_empty() {
        return None;
    }

    let completions = results
        .into_iter()
        .map(|(text, desc, kind)| {
            let ck = match kind {
                SpecCompletionKind::Subcommand => CompletionKind::Subcommand,
                SpecCompletionKind::Option => CompletionKind::Flag,
                SpecCompletionKind::Argument => CompletionKind::Other,
            };
            Completion {
                display: text.clone(),
                text,
                description: desc,
                kind: ck,
                is_dir: false,
            }
        })
        .collect();

    Some(completions)
}

fn complete_spec_args(
    args: &[crate::completion_spec::ArgSpec],
    prefix: &str,
    state: &ShellState,
) -> Vec<Completion> {
    use crate::completion_spec::ArgTemplate;

    let mut completions = Vec::new();
    for arg in args {
        completions.extend(
            arg.suggestions
                .iter()
                .filter(|suggestion| suggestion.starts_with(prefix))
                .map(|suggestion| {
                    project_value_completion(
                        suggestion.clone(),
                        arg.description.as_deref().unwrap_or("option value"),
                    )
                }),
        );
        match &arg.template {
            ArgTemplate::FilePath => completions.extend(complete_path(prefix, state)),
            ArgTemplate::FolderPath => completions.extend(
                complete_path(prefix, state)
                    .into_iter()
                    .filter(|completion| completion.is_dir),
            ),
            ArgTemplate::Generator(name) => {
                completions.extend(complete_from_generator(name, prefix, state))
            }
            ArgTemplate::None => {}
        }
    }
    completions
}

/// Directory names a Python virtual environment conventionally uses.
const VENV_DIR_NAMES: &[&str] = &[".venv", "venv", "env", ".env", "virtualenv"];

/// `source <TAB>` in a project with a virtual environment: the activate
/// script is what someone is reaching for, and it is three segments deep.
/// Offered ahead of the ordinary listing rather than instead of it.
fn complete_venv_activators(state: &ShellState) -> Vec<Completion> {
    let Ok(cwd) = std::env::current_dir() else {
        return Vec::new();
    };
    venv_activators_in(&cwd, state)
}

fn venv_activators_in(cwd: &Path, state: &ShellState) -> Vec<Completion> {
    let mut completions = Vec::new();
    for name in VENV_DIR_NAMES {
        let activate = cwd.join(name).join("bin/activate");
        if !activate.is_file() {
            continue;
        }
        let python = cwd.join(name).join("bin/python");
        let description = if python.is_file() {
            format!("activate {name}")
        } else {
            "activate a virtual environment".to_string()
        };
        completions.push(
            Completion::new(format!("{name}/bin/activate"), CompletionKind::File)
                .with_desc(&description),
        );
    }
    // A venv named in the environment but living elsewhere still activates.
    if let Some(active) = state.env_vars.get("VIRTUAL_ENV") {
        let activate = Path::new(active).join("bin/activate");
        if activate.is_file() {
            let text = escape_shell_word(&activate.to_string_lossy());
            if !completions.iter().any(|item| item.text == text) {
                completions.push(
                    Completion::new(text, CompletionKind::File).with_desc("the active environment"),
                );
            }
        }
    }
    completions
}

/// Installed interpreter versions, from the version managers that keep them
/// in a fixed directory. Reading the directory is the whole probe: asking
/// `nvm`/`pyenv` itself would mean sourcing a shell function on Tab.
fn complete_toolchain_versions(cmd: &str, state: &ShellState) -> Vec<Completion> {
    let home = &state.home_dir;
    let (roots, label): (Vec<PathBuf>, &str) = match cmd {
        "nvm" | "node" | "npx" => (
            vec![state
                .env_vars
                .get("NVM_DIR")
                .map(PathBuf::from)
                .unwrap_or_else(|| home.join(".nvm"))
                .join("versions/node")],
            "installed node",
        ),
        "pyenv" => (
            vec![state
                .env_vars
                .get("PYENV_ROOT")
                .map(PathBuf::from)
                .unwrap_or_else(|| home.join(".pyenv"))
                .join("versions")],
            "installed python",
        ),
        "rbenv" => (vec![home.join(".rbenv/versions")], "installed ruby"),
        "jenv" => (vec![home.join(".jenv/versions")], "installed java"),
        _ => return Vec::new(),
    };

    let mut versions = Vec::new();
    for root in roots {
        let Ok(entries) = fs::read_dir(&root) else {
            continue;
        };
        for entry in entries.flatten().take(MAX_COMPLETION_ITEMS) {
            if !entry.file_type().map(|kind| kind.is_dir()).unwrap_or(false) {
                continue;
            }
            let name = entry.file_name().to_string_lossy().into_owned();
            versions.push(Completion::new(name, CompletionKind::Other).with_desc(label));
        }
    }
    // Newest first, as a version directory listing is not sorted usefully.
    versions.sort_by(|a, b| natural_version_order(&b.text, &a.text));
    versions
}

/// Compare version-shaped names by their numeric parts, so `v20.11.0` sorts
/// above `v9.1.0` where a plain string comparison would not.
fn natural_version_order(a: &str, b: &str) -> std::cmp::Ordering {
    let numbers = |text: &str| {
        text.split(|ch: char| !ch.is_ascii_digit())
            .filter(|part| !part.is_empty())
            .filter_map(|part| part.parse::<u64>().ok())
            .take(4)
            .collect::<Vec<_>>()
    };
    numbers(a).cmp(&numbers(b)).then_with(|| a.cmp(b))
}

/// The dynamic sources a completion spec may name.
///
/// A generator is a fixed name resolved here, never a command line: a spec
/// file is data, and data that could run a program on Tab would make every
/// downloaded spec an execution vector. An unknown name yields nothing, so a
/// spec written for another shell degrades to no suggestions rather than to
/// something surprising.
fn complete_from_generator(name: &str, prefix: &str, state: &ShellState) -> Vec<Completion> {
    let candidates = match name {
        "git_refs" => complete_git_refs(""),
        "git_branches" => complete_git_local_branches(""),
        "git_tags" => complete_git_tags(""),
        "git_remotes" => complete_git_remotes(""),
        "git_worktrees" => complete_git_worktrees(""),
        "git_config_keys" => complete_git_config_keys(""),
        "git_modified_files" => complete_git_dirty_files("", "add"),
        "ssh_hosts" => complete_ssh_hosts("", false, &state.home_dir),
        "users" => complete_users(""),
        "groups" => complete_groups(""),
        "processes" => complete_system_pids(""),
        "signals" => KILL_SIGNALS
            .iter()
            .map(|(signal, desc)| {
                Completion::new(
                    signal.trim_start_matches('-').to_string(),
                    CompletionKind::Other,
                )
                .with_desc(desc)
            })
            .collect(),
        "docker_containers" => complete_docker_containers("", true),
        "docker_running_containers" => complete_docker_containers("", false),
        "docker_images" => complete_docker_images(""),
        "compose_services" => complete_compose_services(""),
        "systemd_units" => complete_systemctl_units("", false, false),
        "systemd_unit_files" => complete_systemctl_units("", false, true),
        "kube_contexts" => complete_kube_names("", KubeName::Context, state),
        "kube_namespaces" => complete_kube_names("", KubeName::Namespace, state),
        "npm_scripts" => complete_npm_scripts(""),
        "npm_dependencies" => complete_npm_dependencies(""),
        "make_targets" => complete_make_targets(""),
        "cargo_bins" => complete_cargo_argument("", CargoArgKind::Bin),
        "cargo_features" => complete_cargo_argument("", CargoArgKind::Feature),
        "cargo_packages" => complete_cargo_argument("", CargoArgKind::Package),
        "environment_variables" => complete_variable_names("", state),
        "shell_functions" => {
            let mut names: Vec<&String> = state.functions.keys().collect();
            names.sort();
            names
                .into_iter()
                .map(|name| {
                    Completion::new(name.clone(), CompletionKind::Function).with_desc("function")
                })
                .collect()
        }
        "bookmarks" => match crate::bookmarks::get_bookmark_db().lock() {
            Ok(db) => db
                .names()
                .into_iter()
                .map(|name| Completion::new(name, CompletionKind::Other).with_desc("bookmark"))
                .collect(),
            Err(_) => Vec::new(),
        },
        _ => Vec::new(),
    };
    rank_prefix_then_fuzzy(candidates, prefix)
}

fn extract_word_at(buf: &str) -> (String, usize) {
    let mut start = 0;
    let mut quote = None;
    let mut escaped = false;
    for (index, ch) in buf.char_indices() {
        if escaped {
            escaped = false;
            continue;
        }
        match quote {
            Some('\'') => {
                if ch == '\'' {
                    quote = None;
                }
            }
            Some('"') => match ch {
                '"' => quote = None,
                '\\' => escaped = true,
                _ => {}
            },
            _ => match ch {
                '\\' => escaped = true,
                '\'' | '"' => quote = Some(ch),
                ' ' | '\t' | '|' | '&' | ';' | '(' | ')' | '<' | '>' => {
                    start = index + ch.len_utf8();
                }
                _ => {}
            },
        }
    }
    let word = buf[start..].to_string();
    (word, start)
}

fn is_command_position(buf: &str, word_start: usize) -> bool {
    let before = buf[..word_start].trim_end_matches([' ', '\t']);
    if before.is_empty()
        || before.ends_with('|')
        || before.ends_with("&&")
        || before.ends_with("||")
        || before.ends_with(';')
        || before.ends_with('\n')
        || before.ends_with('(')
        || before.ends_with('{')
    {
        return true;
    }

    let words: Vec<&str> = active_command_segment(&buf[..word_start])
        .split_whitespace()
        .collect();
    effective_command_index(&words) >= words.len()
}

/// Words that open a shell construct. They are typed where a command is
/// typed, so a command position must offer them; the description is what the
/// construct does, since the word alone rarely says it.
const SHELL_KEYWORDS: &[(&str, &str)] = &[
    ("if", "conditional"),
    ("then", "conditional body"),
    ("else", "conditional alternative"),
    ("elif", "further condition"),
    ("fi", "end a conditional"),
    ("for", "loop over words"),
    ("while", "loop while a command succeeds"),
    ("until", "loop until a command succeeds"),
    ("do", "loop body"),
    ("done", "end a loop"),
    ("case", "match a value"),
    ("esac", "end a match"),
    ("in", "the words of a for or case"),
    ("function", "define a function"),
    ("select", "menu loop"),
    ("time", "time a command"),
];

thread_local! {
    /// Every name that can start a command, built once and filtered per
    /// keystroke. Rebuilding it meant allocating a `Completion` for each of
    /// a few thousand PATH entries on every Tab, which dominated the cost of
    /// completing at command position.
    static COMMAND_CANDIDATES: std::cell::RefCell<Option<(u64, std::rc::Rc<Vec<Completion>>)>> =
        const { std::cell::RefCell::new(None) };
}

/// What the candidate list is built from. When any of it changes the list is
/// stale: a new alias, a `def`, a sourced file of functions, a different
/// PATH. Counting is enough — names are only added or removed as a set, and
/// a rename changes the count of neither more nor less than a swap would.
fn command_candidate_generation(state: &mut ShellState) -> u64 {
    let path_commands = state.path_cache().len() as u64;
    path_commands
        .wrapping_mul(31)
        .wrapping_add(state.aliases.len() as u64)
        .wrapping_mul(31)
        .wrapping_add(state.functions.len() as u64)
        .wrapping_mul(31)
        .wrapping_add(state.user_signatures.len() as u64)
}

fn command_candidates(state: &mut ShellState) -> std::rc::Rc<Vec<Completion>> {
    let generation = command_candidate_generation(state);
    if let Some(cached) = COMMAND_CANDIDATES.with(|cell| {
        cell.borrow()
            .as_ref()
            .filter(|(cached, _)| *cached == generation)
            .map(|(_, candidates)| candidates.clone())
    }) {
        return cached;
    }
    let candidates = std::rc::Rc::new(build_command_candidates(state));
    COMMAND_CANDIDATES.with(|cell| {
        *cell.borrow_mut() = Some((generation, candidates.clone()));
    });
    candidates
}

fn complete_command(prefix: &str, state: &mut ShellState) -> Vec<Completion> {
    let candidates = command_candidates(state);

    // Path completion is per-prefix and cannot be cached with the rest.
    let mut extra = Vec::new();
    if prefix.contains('/') {
        extra = complete_path(prefix, state);
    }

    let pattern = lowered(prefix);
    let mut scored: Vec<(&Completion, i32)> = candidates
        .iter()
        .chain(extra.iter())
        .map(|candidate| {
            (
                candidate,
                fuzzy_match_score_lowered(&candidate.text, &pattern),
            )
        })
        .filter(|(_, score)| *score > 0)
        .collect();
    scored.sort_by(|a, b| {
        b.1.cmp(&a.1)
            .then_with(|| a.0.text.len().cmp(&b.0.text.len()))
    });

    let mut seen = std::collections::HashSet::new();
    scored
        .into_iter()
        .filter(|(candidate, _)| seen.insert(candidate.text.as_str()))
        .take(50)
        .map(|(candidate, _)| candidate.clone())
        .collect()
}

fn build_command_candidates(state: &mut ShellState) -> Vec<Completion> {
    let mut completions = Vec::new();

    for (keyword, description) in SHELL_KEYWORDS {
        completions.push(
            Completion::new((*keyword).to_string(), CompletionKind::Builtin).with_desc(description),
        );
    }

    // Collect all builtin commands
    for cmd in crate::builtins::BUILTIN_NAMES {
        completions.push(Completion {
            text: cmd.to_string(),
            display: cmd.to_string(),
            description: Some("builtin".to_string()),
            kind: CompletionKind::Builtin,
            is_dir: false,
        });
    }

    // Phase 14d: surface signed value-aware builtins (try/each/where/...).
    // Description carries the input → output signature so users can pick the
    // right command by type from the completion list.
    for (name, sig) in crate::signature::SIGNATURES.iter() {
        let desc = format!("{} → {}", sig.input.render(), sig.output.render());
        completions.push(Completion {
            text: (*name).to_string(),
            display: (*name).to_string(),
            description: Some(desc),
            kind: CompletionKind::Builtin,
            is_dir: false,
        });
    }

    // Collect aliases
    for name in state.aliases.keys() {
        completions.push(Completion {
            text: name.clone(),
            display: name.clone(),
            description: Some("alias".to_string()),
            kind: CompletionKind::Alias,
            is_dir: false,
        });
    }

    // Collect functions
    for name in state.functions.keys() {
        completions.push(Completion {
            text: name.clone(),
            display: name.clone(),
            description: Some("function".to_string()),
            kind: CompletionKind::Function,
            is_dir: false,
        });
    }

    // Phase 15c: typed user functions registered via `def`. Description shows
    // the parameter sketch (e.g. "a:int b:string") so completions are useful.
    for (name, sig) in state.user_signatures.iter() {
        let desc = if sig.params.is_empty() {
            "user-defined".to_string()
        } else {
            sig.params
                .iter()
                .map(|p| {
                    format!(
                        "{}{}{}",
                        p.name,
                        if p.optional {
                            "?"
                        } else if p.rest {
                            "..."
                        } else {
                            ""
                        },
                        if matches!(p.kind, crate::signature::Type::Any) {
                            String::new()
                        } else {
                            format!(":{}", p.kind.render())
                        }
                    )
                })
                .collect::<Vec<_>>()
                .join(" ")
        };
        completions.push(Completion {
            text: name.clone(),
            display: name.clone(),
            description: Some(desc),
            kind: CompletionKind::Function,
            is_dir: false,
        });
    }

    // Collect commands in PATH
    for cmd in state.path_cache().iter() {
        completions.push(Completion {
            text: cmd.clone(),
            display: cmd.clone(),
            description: None,
            kind: CompletionKind::Command,
            is_dir: false,
        });
    }

    completions
}

fn path_metadata_desc(entry: &fs::DirEntry) -> Option<String> {
    let ft = entry.file_type().ok()?;
    if ft.is_symlink() {
        let target = fs::read_link(entry.path()).ok()?;
        return Some(format!("→ {}", target.display()));
    }
    if ft.is_dir() {
        let count = fs::read_dir(entry.path()).ok()?.count();
        return Some(format!("{} items", count));
    }
    if ft.is_file() {
        let meta = entry.metadata().ok()?;
        let size = meta.len();
        return Some(format_file_size(size));
    }
    None
}

fn format_file_size(bytes: u64) -> String {
    if bytes < 1024 {
        return format!("{}B", bytes);
    }
    if bytes < 1024 * 1024 {
        return format!("{:.1}K", bytes as f64 / 1024.0);
    }
    if bytes < 1024 * 1024 * 1024 {
        return format!("{:.1}M", bytes as f64 / (1024.0 * 1024.0));
    }
    format!("{:.1}G", bytes as f64 / (1024.0 * 1024.0 * 1024.0))
}

fn complete_path(prefix: &str, state: &ShellState) -> Vec<Completion> {
    let lookup_prefix = unescape_shell_word(prefix);

    // `~alice<TAB>` names a person, not a path: there is nothing to scan
    // until the home directory it stands for is known.
    if let Some(user_prefix) = lookup_prefix
        .strip_prefix('~')
        .filter(|rest| !rest.contains('/'))
    {
        if !user_prefix.is_empty() {
            return complete_user_homes(user_prefix);
        }
    }

    let Some(expanded) = expand_completion_prefix(&lookup_prefix, state) else {
        return Vec::new();
    };

    let (dir, file_prefix) = if expanded.ends_with('/') {
        (expanded.as_str(), "")
    } else {
        match expanded.rfind('/') {
            Some(pos) => (&expanded[..=pos], &expanded[pos + 1..]),
            None => (".", expanded.as_str()),
        }
    };

    // The part of the word the user has already typed, up to and including
    // the last separator. Candidates are built by appending to it verbatim.
    let typed_directory = match prefix.rfind('/') {
        Some(pos) => &prefix[..=pos],
        None => "",
    };

    let mut completions = Vec::new();
    // Case-insensitive prefix matches, used only when nothing matches the
    // typed case exactly (`cd doc<TAB>` → `Documents/`).
    let mut case_fallback = Vec::new();

    if let Ok(entries) = fs::read_dir(dir) {
        for entry in entries.flatten().take(MAX_COMPLETION_ITEMS) {
            let name = entry.file_name().to_string_lossy().to_string();
            let exact = name.starts_with(file_prefix);
            if !exact && !name.to_lowercase().starts_with(&file_prefix.to_lowercase()) {
                continue;
            }
            if name.starts_with('.') && !file_prefix.starts_with('.') {
                continue;
            }

            let is_dir = entry.file_type().map(|t| t.is_dir()).unwrap_or(false);
            // Keep everything the user already typed exactly as they typed
            // it — their own quoting, a `~user`, a `$VAR` — and escape only
            // the name being appended. Re-escaping the typed part would turn
            // `$HOME/` into a literal `\$HOME/`.
            let text = format!(
                "{}{}{}",
                typed_directory,
                escape_shell_word(&name),
                if is_dir { "/" } else { "" }
            );

            let description = path_metadata_desc(&entry);

            let completion = Completion {
                text,
                display: if is_dir {
                    format!("{}/", name)
                } else {
                    name.clone()
                },
                description,
                kind: if is_dir {
                    CompletionKind::Directory
                } else {
                    CompletionKind::File
                },
                is_dir,
            };
            if exact {
                completions.push(completion);
            } else {
                case_fallback.push(completion);
            }
        }
    }

    let mut completions = if completions.is_empty() {
        case_fallback
    } else {
        completions
    };
    completions.sort_by(|a, b| a.text.cmp(&b.text));
    completions
}

fn unescape_shell_word(word: &str) -> String {
    let mut result = String::with_capacity(word.len());
    let mut quote = None;
    let mut chars = word.chars().peekable();
    while let Some(ch) = chars.next() {
        match quote {
            Some('\'') => {
                if ch == '\'' {
                    quote = None;
                } else {
                    result.push(ch);
                }
            }
            Some('"') => match ch {
                '"' => quote = None,
                '\\' => {
                    if let Some(next) = chars.next() {
                        result.push(next);
                    }
                }
                _ => result.push(ch),
            },
            _ => match ch {
                '\'' | '"' => quote = Some(ch),
                '\\' => {
                    if let Some(next) = chars.next() {
                        result.push(next);
                    }
                }
                _ => result.push(ch),
            },
        }
    }
    result
}

fn escape_shell_word(word: &str) -> String {
    let mut result = String::with_capacity(word.len());
    for ch in word.chars() {
        if ch.is_alphanumeric() || matches!(ch, '/' | '_' | '-' | '.' | '~') {
            result.push(ch);
        } else {
            result.push('\\');
            result.push(ch);
        }
    }
    result
}

fn complete_variable(prefix: &str, state: &ShellState) -> Vec<Completion> {
    let mut completions = Vec::new();

    // Add special shell variables first
    let special_vars = vec![
        ("?", "Last exit code"),
        ("!", "Last background PID"),
        ("*", "All positional parameters"),
        ("@", "All positional parameters (quoted)"),
        ("#", "Number of positional parameters"),
        ("0", "Script/shell name"),
        ("-", "Shell options"),
        ("$", "Shell process ID"),
        ("_", "Last command argument"),
    ];

    for (var_name, description) in special_vars {
        if var_name.starts_with(prefix) || prefix.is_empty() {
            completions.push(Completion {
                text: format!("${}", var_name),
                display: var_name.to_string(),
                description: Some(description.to_string()),
                kind: CompletionKind::Variable,
                is_dir: false,
            });
        }
    }

    // Never echo environment values into the completion UI: values commonly
    // contain API keys, access tokens, and other screen-recording-sensitive data.
    let mut env_vars: Vec<_> = state.env_vars.keys().collect();
    env_vars.sort();
    for name in env_vars {
        if name.starts_with(prefix) || prefix.is_empty() {
            completions.push(Completion {
                text: format!("${}", name),
                display: name.clone(),
                description: Some("environment variable".to_string()),
                kind: CompletionKind::Variable,
                is_dir: false,
            });
        }
    }

    // Add local variables from all scopes
    for scope in &state.local_vars_stack {
        let mut local_names: Vec<_> = scope.keys().collect();
        local_names.sort();
        for name in local_names {
            if name.starts_with(prefix) || prefix.is_empty() {
                completions.push(Completion {
                    text: format!("${}", name),
                    display: name.clone(),
                    description: Some("local".to_string()),
                    kind: CompletionKind::Variable,
                    is_dir: false,
                });
            }
        }
    }

    // Add array names
    let mut array_names: Vec<_> = state.arrays.keys().collect();
    array_names.sort();
    for name in array_names {
        if name.starts_with(prefix) || prefix.is_empty() {
            let len = state.array_length(name);
            completions.push(Completion {
                text: format!("${{{}[@]}}", name),
                display: format!("{} [{}]", name, len),
                description: Some(format!("array ({} items)", len)),
                kind: CompletionKind::Variable,
                is_dir: false,
            });
        }
    }

    // Add associative array names
    let mut assoc_names: Vec<_> = state.assoc_arrays.keys().collect();
    assoc_names.sort();
    for name in assoc_names {
        if name.starts_with(prefix) || prefix.is_empty() {
            let len = state.array_length(name);
            completions.push(Completion {
                text: format!("${{{}[@]}}", name),
                display: format!("{} [{}]", name, len),
                description: Some(format!("assoc array ({} items)", len)),
                kind: CompletionKind::Variable,
                is_dir: false,
            });
        }
    }

    // Remove duplicates
    completions.dedup_by(|a, b| a.text == b.text);

    // Apply fuzzy filtering
    let filtered = filter_completions(completions, prefix);
    filtered.into_iter().take(50).collect()
}

pub fn common_prefix(completions: &[Completion]) -> String {
    let Some(first) = completions.first() else {
        return String::new();
    };
    let mut prefix: Vec<char> = first.text.chars().collect();
    for completion in &completions[1..] {
        let common_chars = prefix
            .iter()
            .copied()
            .zip(completion.text.chars())
            .take_while(|(left, right)| left == right)
            .count();
        prefix.truncate(common_chars);
        if prefix.is_empty() {
            break;
        }
    }
    prefix.into_iter().collect()
}

/// Fuzzy match score: higher is better
/// 精确前缀匹配最高分，然后是首字母匹配，最后是子字符串匹配
pub fn fuzzy_match_score(text: &str, pattern: &str) -> i32 {
    fuzzy_match_score_lowered(text, &lowered(pattern))
}

/// Lowercase only when something is actually uppercase. Candidate lists are
/// mostly lowercase command, file and branch names, and this runs once per
/// candidate per keystroke — the allocation is the cost, not the comparison.
fn lowered(text: &str) -> std::borrow::Cow<'_, str> {
    if text.bytes().any(|byte| byte.is_ascii_uppercase()) || !text.is_ascii() {
        std::borrow::Cow::Owned(text.to_lowercase())
    } else {
        std::borrow::Cow::Borrowed(text)
    }
}

/// [`fuzzy_match_score`] with the pattern already lowercased, so a ranking
/// pass over many candidates lowercases it once rather than once per item.
fn fuzzy_match_score_lowered(text: &str, pattern_lower: &str) -> i32 {
    if pattern_lower.is_empty() {
        return 1000; // Empty pattern matches everything with high score
    }

    let text_lower = lowered(text);

    // Exact prefix match: highest score
    if text_lower.starts_with(pattern_lower) {
        return 1000 - (text_lower.len() as i32 - pattern_lower.len() as i32).abs();
    }

    // Check if all characters of pattern exist in text in order
    let mut pattern_chars = pattern_lower.chars().peekable();
    let mut last_match_pos = 0;
    let mut match_count = 0;
    let mut gap_penalty = 0;
    let mut previous_matched = false;

    for (pos, text_char) in text_lower.chars().enumerate() {
        let Some(&pattern_char) = pattern_chars.peek() else {
            break;
        };
        if text_char != pattern_char {
            previous_matched = false;
            continue;
        }
        pattern_chars.next();
        match_count += 1;

        // Penalty for gaps between matches
        gap_penalty += pos.saturating_sub(last_match_pos).saturating_sub(1) as i32;
        last_match_pos = pos;

        // Bonus for a run: this position matched and so did the one before,
        // which is what makes `chk` prefer checkout over cherry-pick.
        if previous_matched {
            gap_penalty = gap_penalty.saturating_sub(5);
        }
        previous_matched = true;
    }

    if match_count == pattern_lower.chars().count() {
        // All characters matched, score based on gaps and position
        500 + (match_count as i32 * 10) - gap_penalty
    } else {
        0 // No match
    }
}

/// Move the candidates this command has had accepted before to the front,
/// highest frecency first, keeping every other candidate in its existing
/// order behind them.
///
/// A stable partition, not a re-sort: the ranking a source chose still holds
/// for everything that has no history, and nothing is added or dropped.
fn promote_accepted(completions: Vec<Completion>, cmd: &str) -> Vec<Completion> {
    if cmd.is_empty() || completions.len() < 2 {
        return completions;
    }
    let Ok(db) = crate::accepted::get_accepted_db().lock() else {
        return completions;
    };
    let scores = db.scores_for(cmd);
    if scores.is_empty() {
        return completions;
    }
    let mut accepted: Vec<(f64, Completion)> = Vec::new();
    let mut rest = Vec::with_capacity(completions.len());
    for completion in completions {
        match scores.get(completion.text.as_str()) {
            Some(score) => accepted.push((*score, completion)),
            None => rest.push(completion),
        }
    }
    accepted.sort_by(|a, b| b.0.partial_cmp(&a.0).unwrap_or(std::cmp::Ordering::Equal));
    let mut ranked: Vec<Completion> = accepted.into_iter().map(|(_, item)| item).collect();
    ranked.extend(rest);
    ranked
}

/// Remember that a completion was taken, so it leads next time. Called from
/// the editor when a candidate is inserted, not when it is merely shown.
pub fn record_accepted(cmd: &str, candidate: &str) {
    if cmd.is_empty() {
        return;
    }
    if let Ok(mut db) = crate::accepted::get_accepted_db().lock() {
        db.record(cmd, candidate);
    }
}

/// The command whose argument is being completed at `cursor`, if any. The
/// editor needs it to record what was accepted.
pub fn command_at(buffer: &str, cursor: usize, state: &ShellState) -> Option<String> {
    let buf = &buffer[..cursor.min(buffer.len())];
    let (_, word_start) = extract_word_at(buf);
    if is_command_position(buf, word_start) {
        return None;
    }
    let segment_start = active_command_segment_start(&buf[..word_start]);
    let expanded = alias_expanded_segment(&buf[segment_start..word_start], &state.aliases);
    let cmd = command_words(expanded.trim()).next()?.to_string();
    (!cmd.is_empty()).then_some(cmd)
}

/// Prefix matches in their source order when any exist; otherwise
/// fuzzy-ranked subsequence matches, so `git chk<TAB>` still finds checkout
/// without prefix matches losing their curated order.
fn rank_prefix_then_fuzzy(completions: Vec<Completion>, pattern: &str) -> Vec<Completion> {
    if pattern.is_empty() || completions.iter().any(|c| c.text.starts_with(pattern)) {
        completions
            .into_iter()
            .filter(|c| c.text.starts_with(pattern))
            .collect()
    } else {
        filter_completions(completions, pattern)
    }
}

/// Filter completions using fuzzy matching
pub fn filter_completions(completions: Vec<Completion>, pattern: &str) -> Vec<Completion> {
    let pattern_lower = lowered(pattern);
    let mut scored: Vec<(Completion, i32)> = completions
        .into_iter()
        .map(|c| {
            let score = fuzzy_match_score_lowered(&c.text, &pattern_lower);
            (c, score)
        })
        .filter(|(_, score)| *score > 0)
        .collect();

    // Sort by score descending, then by text length (shorter is better)
    scored.sort_by(|a, b| {
        let score_cmp = b.1.cmp(&a.1);
        if score_cmp == std::cmp::Ordering::Equal {
            a.0.text.len().cmp(&b.0.text.len())
        } else {
            score_cmp
        }
    });

    scored.into_iter().map(|(c, _)| c).collect()
}

/// Clear the completion cache (useful for tests and cache invalidation)
pub fn clear_cache() {
    COMPLETION_CACHE.with(|cache| {
        cache.borrow_mut().clear();
    });
    PROBE_CACHE.with(|cache| {
        cache.borrow_mut().clear();
    });
    HISTORY_ENTRIES.with(|entries| {
        *entries.borrow_mut() = None;
    });
    // The generation check catches an alias or function added between
    // commands; dropping it here also catches one merely redefined.
    COMMAND_CANDIDATES.with(|candidates| {
        *candidates.borrow_mut() = None;
    });
}

const MAX_HISTORY_ARG_RESULTS: usize = 10;

/// Arguments this command has been given before, for the fallback when no
/// path matches. `git checkout release-2<TAB>` finds the branch spelling from
/// history even though nothing in the working tree matches.
fn complete_history_arguments(cmd: &str, prefix: &str, state: &ShellState) -> Vec<Completion> {
    if cmd.is_empty() {
        return Vec::new();
    }
    let entries = history_entries_once();
    let cwd = std::env::current_dir()
        .ok()
        .map(|p| p.to_string_lossy().to_string());
    history_argument_completions(&entries, cmd, &state.aliases, prefix, cwd.as_deref())
}

thread_local! {
    /// The decoded history for this command line. Reaching the history is
    /// the fallback when no path matches, which is exactly the keystroke
    /// where re-reading and re-parsing the whole file would be felt.
    static HISTORY_ENTRIES: std::cell::RefCell<Option<std::rc::Rc<Vec<crate::history::HistoryEntry>>>> =
        const { std::cell::RefCell::new(None) };
}

fn history_entries_once() -> std::rc::Rc<Vec<crate::history::HistoryEntry>> {
    HISTORY_ENTRIES.with(|cell| {
        if let Some(entries) = cell.borrow().as_ref() {
            return entries.clone();
        }
        let entries = std::rc::Rc::new(crate::history::History::load_default_entries(10_000));
        *cell.borrow_mut() = Some(entries.clone());
        entries
    })
}

/// Pure core of [`complete_history_arguments`]: arguments used with `cmd`
/// anywhere in past command lines, newest first, entries typed in the current
/// directory ahead of the rest.
fn history_argument_completions(
    entries: &[crate::history::HistoryEntry],
    cmd: &str,
    aliases: &HashMap<String, String>,
    prefix: &str,
    cwd: Option<&str>,
) -> Vec<Completion> {
    let mut seen = std::collections::HashSet::new();
    let mut here = Vec::new();
    let mut elsewhere = Vec::new();

    for entry in entries.iter().rev() {
        let same_dir = cwd.is_some() && entry.cwd.as_deref() == cwd;
        for segment in split_command_segments(&entry.command) {
            let words = quote_aware_words(segment);
            let command_index = effective_command_index(&words);
            let Some(head) = words.get(command_index).copied() else {
                continue;
            };
            if resolve_transparent_alias(head.to_string(), aliases) != cmd {
                continue;
            }
            for arg in &words[command_index + 1..] {
                if !arg.starts_with(prefix) || *arg == prefix {
                    continue;
                }
                if !seen.insert(arg.to_string()) {
                    continue;
                }
                let completion = Completion::new(arg.to_string(), CompletionKind::Other).with_desc(
                    if same_dir {
                        "history (this dir)"
                    } else {
                        "history"
                    },
                );
                if same_dir {
                    here.push(completion);
                } else {
                    elsewhere.push(completion);
                }
            }
        }
    }

    here.extend(elsewhere);
    here.truncate(MAX_HISTORY_ARG_RESULTS);
    here
}

/// Split a history line into simple-command segments at unquoted connectors,
/// redirections, and subshell boundaries. Segments that are redirection
/// targets simply fail the head-command comparison later.
fn split_command_segments(line: &str) -> Vec<&str> {
    let mut segments = Vec::new();
    let mut start = 0;
    let mut quote = None;
    let mut escaped = false;
    for (index, ch) in line.char_indices() {
        if escaped {
            escaped = false;
            continue;
        }
        match quote {
            Some('\'') => {
                if ch == '\'' {
                    quote = None;
                }
            }
            Some('"') => match ch {
                '"' => quote = None,
                '\\' => escaped = true,
                _ => {}
            },
            _ => match ch {
                '\\' => escaped = true,
                '\'' | '"' => quote = Some(ch),
                '|' | ';' | '&' | '\n' | '<' | '>' | '(' | ')' => {
                    segments.push(&line[start..index]);
                    start = index + ch.len_utf8();
                }
                _ => {}
            },
        }
    }
    segments.push(&line[start..]);
    segments.retain(|segment| !segment.trim().is_empty());
    segments
}

/// Split a command segment into words at unquoted whitespace, keeping each
/// word's original spelling (quotes and escapes included) so it can be
/// inserted back into a command line as typed.
fn quote_aware_words(segment: &str) -> Vec<&str> {
    let mut words = Vec::new();
    let mut start = None;
    let mut quote = None;
    let mut escaped = false;
    for (index, ch) in segment.char_indices() {
        if escaped {
            escaped = false;
            continue;
        }
        match quote {
            Some('\'') => {
                if ch == '\'' {
                    quote = None;
                }
            }
            Some('"') => match ch {
                '"' => quote = None,
                '\\' => escaped = true,
                _ => {}
            },
            _ => match ch {
                '\\' => escaped = true,
                '\'' | '"' => {
                    quote = Some(ch);
                    start.get_or_insert(index);
                }
                ch if ch.is_whitespace() => {
                    if let Some(word_start) = start.take() {
                        words.push(&segment[word_start..index]);
                    }
                }
                _ => {
                    start.get_or_insert(index);
                }
            },
        }
    }
    if let Some(word_start) = start {
        words.push(&segment[word_start..]);
    }
    words
}

/// Complete history commands based on prefix
/// Returns a list of historical commands sorted by relevance
pub fn complete_from_history(prefix: &str) -> Vec<Completion> {
    let entries = crate::history::History::load_default_entries(10_000);
    complete_history_entries(&entries, prefix)
}

fn complete_history_entries(
    entries: &[crate::history::HistoryEntry],
    prefix: &str,
) -> Vec<Completion> {
    let mut completions = Vec::new();
    let mut seen = std::collections::HashSet::new();

    for entry in entries {
        let cmd = entry.command.split_whitespace().next().unwrap_or("");
        if !cmd.is_empty() && seen.insert(cmd.to_string()) {
            completions.push(Completion {
                text: cmd.to_string(),
                display: cmd.to_string(),
                description: Some("history".to_string()),
                kind: CompletionKind::Command,
                is_dir: false,
            });
        }
    }

    // Reverse to show most recent first, then filter
    completions.reverse();
    filter_completions(completions, prefix)
        .into_iter()
        .take(20)
        .collect()
}

/// Smart pipe completion: recommend pipe targets based on preceding command
pub fn complete_pipe_targets(buf: &str, prefix: &str) -> Vec<Completion> {
    let before_pipe = buf.rsplit_once('|').map(|x| x.0).unwrap_or("").trim();
    let prev_cmd = before_pipe.split_whitespace().next().unwrap_or("");
    let prev_cmd_base = prev_cmd.rsplit('/').next().unwrap_or(prev_cmd);

    let suggestions: &[(&str, &str)] = match prev_cmd_base {
        "cat" | "less" | "head" | "tail" => &[
            ("grep", "Filter lines by pattern"),
            ("wc", "Count lines/words/bytes"),
            ("sort", "Sort lines"),
            ("uniq", "Remove duplicates"),
            ("awk", "Text processing"),
            ("sed", "Stream editing"),
            ("cut", "Extract columns"),
            ("tr", "Translate characters"),
        ],
        "curl" | "wget" => &[
            ("jq", "JSON processor"),
            ("grep", "Filter output"),
            ("python3 -m json.tool", "Pretty-print JSON"),
            ("tee", "Write and pass through"),
        ],
        "find" => &[
            ("xargs", "Execute on results"),
            ("grep", "Filter results"),
            ("sort", "Sort results"),
            ("wc -l", "Count results"),
            ("head", "First N results"),
        ],
        "ps" => &[
            ("grep", "Filter processes"),
            ("awk", "Extract columns"),
            ("sort", "Sort output"),
            ("head", "Top entries"),
        ],
        "ls" | "dir" => &[
            ("grep", "Filter files"),
            ("sort", "Sort output"),
            ("wc -l", "Count entries"),
            ("head", "First entries"),
        ],
        "docker" => &[
            ("grep", "Filter output"),
            ("awk", "Extract fields"),
            ("jq", "JSON processing"),
            ("xargs", "Execute on results"),
        ],
        "echo" | "printf" => &[
            ("tr", "Translate characters"),
            ("sed", "Stream editing"),
            ("base64", "Encode/decode"),
            ("xclip", "Copy to clipboard"),
        ],
        "git" => &[
            ("grep", "Filter output"),
            ("head", "First N lines"),
            ("wc -l", "Count lines"),
            ("sort", "Sort output"),
        ],
        "df" | "du" => &[
            ("sort -h", "Sort by size"),
            ("grep", "Filter output"),
            ("tail", "Last entries"),
            ("awk", "Extract columns"),
        ],
        _ => &[
            ("grep", "Filter by pattern"),
            ("sort", "Sort output"),
            ("head", "First N lines"),
            ("tail", "Last N lines"),
            ("wc", "Count lines/words"),
            ("awk", "Text processing"),
            ("xargs", "Execute on each line"),
            ("tee", "Write and pass through"),
        ],
    };

    let mut completions = Vec::new();
    for &(cmd, desc) in suggestions {
        if prefix.is_empty() || cmd.starts_with(prefix) {
            completions.push(Completion {
                text: cmd.to_string(),
                display: cmd.to_string(),
                description: Some(desc.to_string()),
                kind: CompletionKind::Command,
                is_dir: false,
            });
        }
    }
    completions
}

/// Detect project type and provide context-aware completions
fn find_upwards(name: &str) -> Option<std::path::PathBuf> {
    let cwd = std::env::current_dir().ok()?;
    find_upwards_from(&cwd, name)
}

fn find_upwards_from(start: &std::path::Path, name: &str) -> Option<std::path::PathBuf> {
    start
        .ancestors()
        .map(|dir| dir.join(name))
        .find(|path| path.is_file())
}

fn project_value_completion(text: String, description: impl Into<String>) -> Completion {
    Completion {
        display: text.clone(),
        text,
        description: Some(description.into()),
        kind: CompletionKind::Other,
        is_dir: false,
    }
}

fn complete_npm_scripts(prefix: &str) -> Vec<Completion> {
    let Some(path) = find_upwards("package.json") else {
        return Vec::new();
    };
    npm_scripts_from_path(&path, prefix)
}

fn npm_scripts_from_path(path: &std::path::Path, prefix: &str) -> Vec<Completion> {
    let Ok(content) = crate::io_guard::read_regular_text(path, MAX_COMPLETION_PROJECT_FILE_BYTES)
    else {
        return Vec::new();
    };
    let Ok(json) = serde_json::from_str::<serde_json::Value>(&content) else {
        return Vec::new();
    };
    json.get("scripts")
        .and_then(serde_json::Value::as_object)
        .into_iter()
        .flatten()
        .filter(|(name, _)| name.starts_with(prefix))
        .map(|(name, command)| {
            project_value_completion(
                name.clone(),
                command.as_str().unwrap_or("package.json script"),
            )
        })
        .collect()
}

fn node_script_command(package_json: &std::path::Path, script: &str) -> String {
    let root = package_json
        .parent()
        .unwrap_or_else(|| std::path::Path::new("."));
    let runner = if root.join("pnpm-lock.yaml").is_file() {
        "pnpm run"
    } else if root.join("yarn.lock").is_file() {
        "yarn"
    } else if root.join("bun.lock").is_file() || root.join("bun.lockb").is_file() {
        "bun run"
    } else {
        "npm run"
    };
    format!("{} {}", runner, script)
}

#[derive(Clone, Copy)]
enum CargoArgKind {
    Package,
    Bin,
    Example,
    Feature,
}

fn complete_cargo_argument(prefix: &str, kind: CargoArgKind) -> Vec<Completion> {
    let Some(manifest) = find_upwards("Cargo.toml") else {
        return Vec::new();
    };
    cargo_values_from_manifest(&manifest, prefix, kind)
        .into_iter()
        .map(|value| {
            let description = match kind {
                CargoArgKind::Package => "workspace package",
                CargoArgKind::Bin => "binary target",
                CargoArgKind::Example => "example target",
                CargoArgKind::Feature => "Cargo feature",
            };
            project_value_completion(value, description)
        })
        .collect()
}

fn cargo_values_from_manifest(
    manifest_path: &std::path::Path,
    prefix: &str,
    kind: CargoArgKind,
) -> Vec<String> {
    let Ok(content) =
        crate::io_guard::read_regular_text(manifest_path, MAX_COMPLETION_PROJECT_FILE_BYTES)
    else {
        return Vec::new();
    };
    let Ok(manifest) = toml::from_str::<toml::Value>(&content) else {
        return Vec::new();
    };
    let root = manifest_path
        .parent()
        .unwrap_or_else(|| std::path::Path::new("."));
    let mut values = Vec::new();

    match kind {
        CargoArgKind::Feature => {
            if let Some(features) = manifest.get("features").and_then(toml::Value::as_table) {
                values.extend(features.keys().cloned());
            }
        }
        CargoArgKind::Bin => {
            if root.join("src/main.rs").is_file() {
                if let Some(name) = manifest
                    .get("package")
                    .and_then(|package| package.get("name"))
                    .and_then(toml::Value::as_str)
                {
                    values.push(name.to_string());
                }
            }
            if let Some(bins) = manifest.get("bin").and_then(toml::Value::as_array) {
                values.extend(bins.iter().filter_map(|bin| {
                    bin.get("name")
                        .and_then(toml::Value::as_str)
                        .map(str::to_string)
                }));
            }
            values.extend(rust_target_names(&root.join("src/bin")));
        }
        CargoArgKind::Example => values.extend(rust_target_names(&root.join("examples"))),
        CargoArgKind::Package => {
            if let Some(name) = manifest
                .get("package")
                .and_then(|package| package.get("name"))
                .and_then(toml::Value::as_str)
            {
                values.push(name.to_string());
            }
            if let Some(members) = manifest
                .get("workspace")
                .and_then(|workspace| workspace.get("members"))
                .and_then(toml::Value::as_array)
            {
                for member in members.iter().filter_map(toml::Value::as_str) {
                    let pattern = root.join(member).join("Cargo.toml");
                    let Some(pattern) = pattern.to_str() else {
                        continue;
                    };
                    if let Ok(paths) = glob::glob(pattern) {
                        for path in paths.flatten().take(MAX_COMPLETION_ITEMS) {
                            if let Ok(content) = crate::io_guard::read_regular_text(
                                &path,
                                MAX_COMPLETION_PROJECT_FILE_BYTES,
                            ) {
                                if let Ok(value) = toml::from_str::<toml::Value>(&content) {
                                    if let Some(name) = value
                                        .get("package")
                                        .and_then(|package| package.get("name"))
                                        .and_then(toml::Value::as_str)
                                    {
                                        values.push(name.to_string());
                                    }
                                }
                            }
                        }
                    }
                }
            }
        }
    }

    values.sort();
    values.dedup();
    let (base, partial) = if matches!(kind, CargoArgKind::Feature) {
        prefix.rsplit_once(',').unwrap_or(("", prefix))
    } else {
        ("", prefix)
    };
    values
        .into_iter()
        .filter(|value| value.starts_with(partial))
        .map(|value| {
            if base.is_empty() {
                value
            } else {
                format!("{},{}", base, value)
            }
        })
        .collect()
}

fn rust_target_names(dir: &std::path::Path) -> Vec<String> {
    let Ok(entries) = fs::read_dir(dir) else {
        return Vec::new();
    };
    entries
        .flatten()
        .take(MAX_COMPLETION_ITEMS)
        .filter_map(|entry| {
            let path = entry.path();
            if path.is_file() && path.extension().is_some_and(|ext| ext == "rs") {
                path.file_stem()
                    .map(|name| name.to_string_lossy().into_owned())
            } else if path.is_dir() && path.join("main.rs").is_file() {
                path.file_name()
                    .map(|name| name.to_string_lossy().into_owned())
            } else {
                None
            }
        })
        .collect()
}

fn complete_make_targets(prefix: &str) -> Vec<Completion> {
    let makefile = ["Makefile", "makefile", "GNUmakefile"]
        .into_iter()
        .find_map(find_upwards);
    let Some(makefile) = makefile else {
        return Vec::new();
    };
    make_targets_from_path(&makefile, prefix)
        .into_iter()
        .map(|target| project_value_completion(target, "Makefile target"))
        .collect()
}

fn make_targets_from_path(path: &std::path::Path, prefix: &str) -> Vec<String> {
    let Ok(content) = crate::io_guard::read_regular_text(path, MAX_COMPLETION_PROJECT_FILE_BYTES)
    else {
        return Vec::new();
    };
    let mut targets = Vec::new();
    for line in content.lines() {
        if targets.len() >= MAX_COMPLETION_ITEMS {
            break;
        }
        if line.chars().next().is_some_and(char::is_whitespace) || line.starts_with('#') {
            continue;
        }
        let Some((names, remainder)) = line.split_once(':') else {
            continue;
        };
        if remainder.trim_start().starts_with('=') {
            continue;
        }
        for target in names.split_whitespace() {
            if target.starts_with(prefix)
                && !target.starts_with('.')
                && !target.contains('%')
                && !target.contains('=')
            {
                targets.push(target.to_string());
            }
        }
    }
    targets.sort();
    targets.dedup();
    targets
}

pub fn complete_project_commands(prefix: &str) -> Vec<Completion> {
    let mut completions = Vec::new();
    let cwd = match std::env::current_dir() {
        Ok(p) => p,
        Err(_) => return completions,
    };

    // Cargo.toml → Rust project
    if find_upwards_from(&cwd, "Cargo.toml").is_some() {
        let rust_cmds: &[(&str, &str)] = &[
            ("cargo build", "Build the project"),
            ("cargo test", "Run tests"),
            ("cargo run", "Run the project"),
            ("cargo check", "Check for errors"),
            ("cargo clippy", "Run linter"),
            ("cargo fmt", "Format code"),
            ("cargo doc", "Build documentation"),
            ("cargo bench", "Run benchmarks"),
        ];
        for &(cmd, desc) in rust_cmds {
            if prefix.is_empty() || cmd.starts_with(prefix) {
                completions.push(Completion {
                    text: cmd.to_string(),
                    display: cmd.to_string(),
                    description: Some(desc.to_string()),
                    kind: CompletionKind::Command,
                    is_dir: false,
                });
            }
        }
    }

    // package.json → Node project
    if let Some(package_json) = find_upwards_from(&cwd, "package.json") {
        for script in npm_scripts_from_path(&package_json, "") {
            let cmd = node_script_command(&package_json, &script.text);
            if prefix.is_empty() || cmd.starts_with(prefix) {
                completions.push(Completion {
                    text: cmd.clone(),
                    display: cmd,
                    description: script.description,
                    kind: CompletionKind::Command,
                    is_dir: false,
                });
            }
        }
    }

    // Makefile → Make targets
    let makefile = ["Makefile", "makefile", "GNUmakefile"]
        .into_iter()
        .find_map(|name| find_upwards_from(&cwd, name));
    if let Some(mf_path) = makefile {
        for target in make_targets_from_path(&mf_path, "") {
            let cmd = format!("make {}", target);
            if prefix.is_empty() || cmd.starts_with(prefix) {
                completions.push(Completion {
                    text: cmd.clone(),
                    display: cmd,
                    description: Some("Makefile target".to_string()),
                    kind: CompletionKind::Command,
                    is_dir: false,
                });
            }
        }
    }

    // pyproject.toml or setup.py → Python project
    if find_upwards_from(&cwd, "pyproject.toml").is_some()
        || find_upwards_from(&cwd, "setup.py").is_some()
    {
        let py_cmds: &[(&str, &str)] = &[
            ("python -m pytest", "Run tests"),
            ("pip install -e .", "Install in dev mode"),
            ("python -m mypy .", "Type check"),
            ("python -m black .", "Format code"),
        ];
        for &(cmd, desc) in py_cmds {
            if prefix.is_empty() || cmd.starts_with(prefix) {
                completions.push(Completion {
                    text: cmd.to_string(),
                    display: cmd.to_string(),
                    description: Some(desc.to_string()),
                    kind: CompletionKind::Command,
                    is_dir: false,
                });
            }
        }
    }

    // go.mod → Go project
    if find_upwards_from(&cwd, "go.mod").is_some() {
        let go_cmds: &[(&str, &str)] = &[
            ("go build ./...", "Build all packages"),
            ("go test ./...", "Run all tests"),
            ("go run .", "Run the project"),
            ("go vet ./...", "Check for issues"),
            ("go mod tidy", "Clean up dependencies"),
        ];
        for &(cmd, desc) in go_cmds {
            if prefix.is_empty() || cmd.starts_with(prefix) {
                completions.push(Completion {
                    text: cmd.to_string(),
                    display: cmd.to_string(),
                    description: Some(desc.to_string()),
                    kind: CompletionKind::Command,
                    is_dir: false,
                });
            }
        }
    }

    completions
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn variable_completion_never_displays_environment_values() {
        let mut state = ShellState::new(false);
        state.env_vars.insert(
            "JSH_TEST_SECRET".to_string(),
            "super-secret-token".to_string(),
        );

        let completions = complete_variable("JSH_TEST_SECRET", &state);
        let secret = completions
            .iter()
            .find(|completion| completion.text == "$JSH_TEST_SECRET")
            .unwrap();
        assert_eq!(secret.description.as_deref(), Some("environment variable"));
        assert!(!format!("{:?}", secret.description).contains("super-secret-token"));
    }

    #[test]
    fn completion_boundary_rejects_hidden_insertions_and_escapes_metadata() {
        let completions = finalize_completions(vec![
            Completion {
                text: "safe".into(),
                display: "safe\x1b]52;c;bad\x07".into(),
                description: Some("left\u{202e}right".into()),
                kind: CompletionKind::Other,
                is_dir: false,
            },
            Completion {
                text: "hidden\u{2066}".into(),
                display: "hidden".into(),
                description: None,
                kind: CompletionKind::Other,
                is_dir: false,
            },
        ]);
        assert_eq!(completions.len(), 1);
        assert_eq!(completions[0].text, "safe");
        assert!(completions[0].display.contains("\\x1b"));
        assert!(completions[0]
            .description
            .as_deref()
            .is_some_and(|value| value.contains("\\u{202e}")));
    }

    #[test]
    fn common_prefix_is_unicode_safe() {
        let cjk = [
            Completion::new("你好".to_string(), CompletionKind::File),
            Completion::new("你们".to_string(), CompletionKind::File),
        ];
        assert_eq!(common_prefix(&cjk), "你");

        let emoji = [
            Completion::new("🦀cargo".to_string(), CompletionKind::Command),
            Completion::new("🦀cache".to_string(), CompletionKind::Command),
        ];
        assert_eq!(common_prefix(&emoji), "🦀ca");
    }

    #[test]
    fn history_completion_consumes_decoded_entries() {
        let entries = vec![crate::history::HistoryEntry {
            command: "git status --short".to_string(),
            timestamp: 1_700_000_000,
            cwd: Some("/workspace".to_string()),
        }];

        let completions = complete_history_entries(&entries, "git");
        assert_eq!(completions.len(), 1);
        assert_eq!(completions[0].text, "git");
        assert!(!completions[0].text.contains("1700000000"));
        assert!(!completions[0].text.contains("/workspace"));
    }

    #[test]
    fn git_dynamic_arguments_do_not_hide_spec_flags() {
        clear_cache();
        let mut state = ShellState::new(false);
        let buffer = "git push -";
        let (_, completions) = complete(buffer, buffer.len(), &mut state);
        assert!(completions.iter().any(|item| item.text == "--force"));
        assert!(completions
            .iter()
            .any(|item| item.text == "--force-with-lease"));

        clear_cache();
        let buffer = "git checkout -";
        let (_, completions) = complete(buffer, buffer.len(), &mut state);
        assert!(completions.iter().any(|item| item.text == "-b"));
        assert!(completions.iter().any(|item| item.text == "--track"));
    }

    #[test]
    fn remote_branch_shortening_supports_any_remote_and_skips_head() {
        assert_eq!(
            split_remote_branch("upstream/feature/smart-completion"),
            Some(("upstream", "feature/smart-completion"))
        );
        assert_eq!(split_remote_branch("origin/HEAD"), None);
    }

    #[test]
    fn shell_word_escaping_round_trips_paths_with_spaces() {
        let path = "docs/release notes (final).md";
        let escaped = escape_shell_word(path);
        assert_eq!(escaped, "docs/release\\ notes\\ \\(final\\).md");
        assert_eq!(unescape_shell_word(&escaped), path);
        assert_eq!(
            unescape_shell_word("'docs/release notes'"),
            "docs/release notes"
        );
        assert_eq!(
            extract_word_at("cat docs/release\\ notes"),
            ("docs/release\\ notes".to_string(), 4)
        );
        assert_eq!(
            extract_word_at("cat \"docs/release notes"),
            ("\"docs/release notes".to_string(), 4)
        );
    }

    #[test]
    fn porcelain_z_parser_keeps_rename_destination_and_spaces() {
        let output = b" M file one.txt\0R  new name.txt\0old name.txt\0?? next file.txt\0";
        let entries = parse_git_status_entries(output);
        assert_eq!(entries.len(), 3);
        assert_eq!(entries[0], ([b' ', b'M'], "file one.txt".to_string()));
        assert_eq!(entries[1], ([b'R', b' '], "new name.txt".to_string()));
        assert_eq!(entries[2], ([b'?', b'?'], "next file.txt".to_string()));
    }

    #[test]
    fn git_ref_parser_combines_refs_and_deduplicates_remote_branches() {
        let refs = "refs/heads/main\nrefs/remotes/origin/HEAD\nrefs/remotes/origin/main\nrefs/remotes/upstream/feature/x\nrefs/tags/v1.0\n";
        let completions = parse_git_refs(refs, "");
        assert_eq!(
            completions
                .iter()
                .map(|item| item.text.as_str())
                .collect::<Vec<_>>(),
            vec!["main", "feature/x", "v1.0"]
        );
        assert_eq!(
            completions[1].description.as_deref(),
            Some("remote (upstream)")
        );
    }

    #[test]
    fn git_file_completion_respects_index_and_worktree_columns() {
        assert_eq!(git_file_description([b'M', b' '], "add"), None);
        assert_eq!(git_file_description([b' ', b'M'], "add"), Some("modified"));
        assert_eq!(git_file_description([b'M', b' '], "restore"), None);
        assert_eq!(
            git_file_description([b'M', b' '], "restore_staged"),
            Some("staged")
        );
        assert_eq!(git_file_description([b'?', b'?'], "restore"), None);
    }

    #[test]
    fn project_files_are_discovered_from_nested_directories() {
        let tmp = tempfile::tempdir().unwrap();
        let nested = tmp.path().join("src/deep");
        fs::create_dir_all(&nested).unwrap();
        fs::write(tmp.path().join("package.json"), "{\"scripts\":{}} ").unwrap();
        assert_eq!(
            find_upwards_from(&nested, "package.json"),
            Some(tmp.path().join("package.json"))
        );
    }

    #[test]
    fn npm_scripts_and_make_targets_are_project_native_arguments() {
        let tmp = tempfile::tempdir().unwrap();
        let package = tmp.path().join("package.json");
        fs::write(
            &package,
            r#"{"scripts":{"build":"vite build","test:unit":"vitest"}}"#,
        )
        .unwrap();
        let scripts = npm_scripts_from_path(&package, "test");
        assert_eq!(scripts.len(), 1);
        assert_eq!(scripts[0].text, "test:unit");
        assert_eq!(scripts[0].description.as_deref(), Some("vitest"));

        let makefile = tmp.path().join("Makefile");
        fs::write(
            &makefile,
            "# comment\nMODE := release\nbuild test: deps\n.PHONY: build\npattern-%:\n\t@echo ignored\n",
        )
        .unwrap();
        assert_eq!(
            make_targets_from_path(&makefile, ""),
            vec!["build".to_string(), "test".to_string()]
        );

        fs::write(tmp.path().join("pnpm-lock.yaml"), "lockfileVersion: 9").unwrap();
        assert_eq!(node_script_command(&package, "build"), "pnpm run build");
        fs::remove_file(tmp.path().join("pnpm-lock.yaml")).unwrap();
        fs::write(tmp.path().join("yarn.lock"), "").unwrap();
        assert_eq!(node_script_command(&package, "build"), "yarn build");
    }

    #[test]
    fn cargo_manifest_completes_bins_examples_features_and_packages() {
        let tmp = tempfile::tempdir().unwrap();
        fs::create_dir_all(tmp.path().join("src/bin")).unwrap();
        fs::create_dir_all(tmp.path().join("examples")).unwrap();
        fs::create_dir_all(tmp.path().join("crates/helper/src")).unwrap();
        fs::write(tmp.path().join("src/main.rs"), "fn main() {}").unwrap();
        fs::write(tmp.path().join("src/bin/admin.rs"), "fn main() {}").unwrap();
        fs::write(tmp.path().join("examples/demo.rs"), "fn main() {}").unwrap();
        fs::write(
            tmp.path().join("Cargo.toml"),
            "[package]\nname='app'\nversion='0.1.0'\n[features]\ndefault=[]\nserde=[]\n[workspace]\nmembers=['crates/*']\n",
        )
        .unwrap();
        fs::write(
            tmp.path().join("crates/helper/Cargo.toml"),
            "[package]\nname='helper'\nversion='0.1.0'\n",
        )
        .unwrap();
        let manifest = tmp.path().join("Cargo.toml");

        assert_eq!(
            cargo_values_from_manifest(&manifest, "", CargoArgKind::Bin),
            vec!["admin".to_string(), "app".to_string()]
        );
        assert_eq!(
            cargo_values_from_manifest(&manifest, "d", CargoArgKind::Example),
            vec!["demo".to_string()]
        );
        assert_eq!(
            cargo_values_from_manifest(&manifest, "default,s", CargoArgKind::Feature),
            vec!["default,serde".to_string()]
        );
        assert_eq!(
            cargo_values_from_manifest(&manifest, "h", CargoArgKind::Package),
            vec!["helper".to_string()]
        );
    }

    #[test]
    fn active_command_segment_handles_connectors_quotes_and_subshells() {
        assert_eq!(
            active_command_segment("echo 'x; y' && git push"),
            "git push"
        );
        assert_eq!(
            active_command_segment("echo $(printf 'a;b') && cargo run"),
            "cargo run"
        );
        assert_eq!(active_command_segment("echo x | grep y"), "grep y");
        assert_eq!(first_command("RUST_LOG=debug cargo test"), "cargo");
    }

    #[test]
    fn completion_routes_to_the_command_after_connectors() {
        clear_cache();
        let mut state = ShellState::new(false);
        let buffer = "echo ok && git pu";
        let (_, completions) = complete(buffer, buffer.len(), &mut state);
        assert!(completions.iter().any(|item| item.text == "push"));

        clear_cache();
        let buffer = "echo ok; git push -";
        let (_, completions) = complete(buffer, buffer.len(), &mut state);
        assert!(completions.iter().any(|item| item.text == "--force"));

        assert!(is_command_position("echo ok\ncar", "echo ok\n".len()));

        let word_start = "false || ".len();
        let before = "false || "[..word_start].trim_end();
        assert!(!before.ends_with('|') || before.ends_with("||"));
    }

    #[test]
    fn spec_option_values_complete_separate_and_inline_forms() {
        clear_cache();
        let mut state = ShellState::new(false);

        let buffer = "npm publish --access p";
        let (_, completions) = complete(buffer, buffer.len(), &mut state);
        assert!(completions.iter().any(|item| item.text == "public"));

        clear_cache();
        let buffer = "npm publish --access=p";
        let (_, completions) = complete(buffer, buffer.len(), &mut state);
        assert!(completions
            .iter()
            .any(|item| item.text == "--access=public"));

        clear_cache();
        let buffer = "cargo build --features=a";
        let (_, completions) = complete(buffer, buffer.len(), &mut state);
        assert!(completions.iter().any(|item| item.text == "--features=ai"));

        clear_cache();
        let buffer = "cargo install --path s";
        let (_, completions) = complete(buffer, buffer.len(), &mut state);
        assert!(completions.iter().any(|item| item.text == "src/"));
    }

    #[test]
    fn wrappers_assignments_and_simple_aliases_route_to_the_real_command() {
        assert_eq!(first_command("sudo git push"), "git");
        assert_eq!(
            first_command("sudo -u root env RUST_LOG=debug cargo test"),
            "cargo"
        );
        assert_eq!(first_command("time -p command git status"), "git");

        let mut state = ShellState::new(false);
        state.aliases.insert("g".into(), "git".into());

        clear_cache();
        let buffer = "sudo git pu";
        let (_, completions) = complete(buffer, buffer.len(), &mut state);
        assert!(completions.iter().any(|item| item.text == "push"));

        clear_cache();
        let buffer = "RUST_LOG=debug car";
        let (_, completions) = complete(buffer, buffer.len(), &mut state);
        assert!(completions.iter().any(|item| item.text == "cargo"));

        clear_cache();
        let buffer = "g pu";
        let (_, completions) = complete(buffer, buffer.len(), &mut state);
        assert!(completions.iter().any(|item| item.text == "push"));

        clear_cache();
        let buffer = "sudo git push -";
        let (_, completions) = complete(buffer, buffer.len(), &mut state);
        assert!(completions.iter().any(|item| item.text == "--force"));
    }

    #[test]
    fn ssh_config_parser_keeps_aliases_and_drops_patterns() {
        let config = "# comment\n\
            Host dev prod-*\n\
            \tHostName dev.example.com\n\
            host staging bastion\n\
            Match host other\n\
            Host !deny secret?\n";
        assert_eq!(
            parse_ssh_config_hosts(config),
            vec!["dev", "staging", "bastion"]
        );
    }

    #[test]
    fn known_hosts_parser_skips_hashed_and_ipv6_and_unwraps_ports() {
        let known = "github.com ssh-ed25519 AAAA\n\
            |1|hash|hash= ssh-rsa AAAA\n\
            @cert-authority *.corp.example ssh-rsa AAAA\n\
            [bastion.example]:2222,10.0.0.5 ecdsa-sha2-nistp256 AAAA\n\
            fe80::1 ssh-ed25519 AAAA\n\
            # comment\n";
        assert_eq!(
            parse_known_hosts(known),
            vec!["github.com", "bastion.example", "10.0.0.5"]
        );
    }

    #[test]
    fn ssh_family_completes_hosts_preserving_user_and_scp_colon() {
        let tmp = tempfile::tempdir().unwrap();
        fs::create_dir_all(tmp.path().join(".ssh")).unwrap();
        fs::write(
            tmp.path().join(".ssh/config"),
            "Host devbox\n  HostName dev.example.com\n",
        )
        .unwrap();
        fs::write(
            tmp.path().join(".ssh/known_hosts"),
            "devbox ssh-ed25519 AAAA\nweb.example.com ssh-ed25519 AAAA\n",
        )
        .unwrap();

        let hosts = complete_ssh_hosts("dev", false, tmp.path());
        assert_eq!(hosts.len(), 1, "config and known_hosts entries deduplicate");
        assert_eq!(hosts[0].text, "devbox");
        assert_eq!(hosts[0].description.as_deref(), Some("ssh config"));

        let hosts = complete_ssh_hosts("root@", true, tmp.path());
        let texts: Vec<&str> = hosts.iter().map(|c| c.text.as_str()).collect();
        assert!(texts.contains(&"root@devbox:"));
        assert!(texts.contains(&"root@web.example.com:"));

        assert!(complete_ssh_hosts("devbox:/etc", false, tmp.path()).is_empty());
    }

    #[test]
    fn ssh_offers_hosts_only_until_a_destination_exists() {
        assert_eq!(ssh_positional_count(&["ssh"]), 0);
        assert_eq!(ssh_positional_count(&["ssh", "-p", "2222"]), 0);
        assert_eq!(ssh_positional_count(&["ssh", "-v", "devbox"]), 1);
        assert_eq!(ssh_positional_count(&["ssh", "devbox", "ls"]), 2);
    }

    #[test]
    fn declaration_builtins_complete_bare_names() {
        let mut state = ShellState::new(false);
        state
            .env_vars
            .insert("JSH_COMPLETE_ME".to_string(), "value".to_string());

        clear_cache();
        let buffer = "export JSH_COMPLE";
        let (_, completions) = complete(buffer, buffer.len(), &mut state);
        assert!(completions
            .iter()
            .any(|item| item.text == "JSH_COMPLETE_ME"));
        assert!(completions.iter().all(|item| !item.text.starts_with('$')));

        state.functions.insert(
            "deploy_widgets".to_string(),
            crate::parser::ast::CompoundCommand::BraceGroup {
                body: Vec::new(),
                redirects: Vec::new(),
            },
        );
        clear_cache();
        let buffer = "unset -f deploy";
        let (_, completions) = complete(buffer, buffer.len(), &mut state);
        assert!(completions.iter().any(|item| item.text == "deploy_widgets"));
    }

    #[test]
    fn unalias_completes_alias_names_with_expansions() {
        let mut state = ShellState::new(false);
        state
            .aliases
            .insert("gs".to_string(), "git status".to_string());

        clear_cache();
        let buffer = "unalias g";
        let (_, completions) = complete(buffer, buffer.len(), &mut state);
        let alias = completions.iter().find(|item| item.text == "gs").unwrap();
        assert_eq!(alias.description.as_deref(), Some("alias for git status"));
    }

    #[test]
    fn which_and_type_complete_command_names() {
        let mut state = ShellState::new(false);
        clear_cache();
        let buffer = "which ech";
        let (_, completions) = complete(buffer, buffer.len(), &mut state);
        assert!(completions.iter().any(|item| item.text == "echo"));
    }

    #[test]
    fn job_control_completes_specs_and_kill_completes_signals() {
        let mut state = ShellState::new(false);
        state
            .jobs
            .add(nix::unistd::Pid::from_raw(4242), "sleep 100".to_string());

        clear_cache();
        let buffer = "fg %";
        let (_, completions) = complete(buffer, buffer.len(), &mut state);
        let job = completions.iter().find(|item| item.text == "%1").unwrap();
        assert!(job.description.as_deref().unwrap().contains("sleep 100"));

        clear_cache();
        let buffer = "kill 42";
        let (_, completions) = complete(buffer, buffer.len(), &mut state);
        assert!(completions.iter().any(|item| item.text == "4242"));

        clear_cache();
        let buffer = "kill -";
        let (_, completions) = complete(buffer, buffer.len(), &mut state);
        assert!(completions.iter().any(|item| item.text == "-9"));
        assert!(completions.iter().any(|item| item.text == "-TERM"));
    }

    #[test]
    fn braced_variables_complete_closed() {
        let mut state = ShellState::new(false);
        state
            .env_vars
            .insert("JSH_BRACED_VAR".to_string(), "value".to_string());

        clear_cache();
        let buffer = "echo ${JSH_BRACED";
        let (_, completions) = complete(buffer, buffer.len(), &mut state);
        assert!(completions
            .iter()
            .any(|item| item.text == "${JSH_BRACED_VAR}"));
    }

    #[test]
    fn assignment_values_complete_as_paths() {
        assert_eq!(assignment_value_start("DEST=sr"), Some(5));
        assert_eq!(assignment_value_start("PATH+=/usr"), Some(6));
        assert_eq!(assignment_value_start("--bin=x"), None);
        assert_eq!(assignment_value_start("no-equals"), None);

        let mut state = ShellState::new(false);
        clear_cache();
        let buffer = "export DEST=sr";
        let (_, completions) = complete(buffer, buffer.len(), &mut state);
        assert!(completions.iter().any(|item| item.text == "DEST=src/"));
    }

    #[test]
    fn history_arguments_complete_for_the_same_command_newest_and_here_first() {
        let entry = |command: &str, cwd: Option<&str>| crate::history::HistoryEntry {
            command: command.to_string(),
            timestamp: 0,
            cwd: cwd.map(|s| s.to_string()),
        };
        let entries = vec![
            entry("git checkout release-2.1", Some("/elsewhere")),
            entry("sudo git checkout hotfix-branch", Some("/elsewhere")),
            entry("cat notes.txt | git checkout feature-x", Some("/here")),
            entry("git checkout main", Some("/here")),
            entry("echo 'git checkout not-a-command'", Some("/here")),
        ];
        let aliases = HashMap::new();

        let completions =
            history_argument_completions(&entries, "git", &aliases, "", Some("/here"));
        let texts: Vec<&str> = completions.iter().map(|c| c.text.as_str()).collect();
        // Same-directory entries lead, newest first; the quoted echo payload
        // is not treated as a git invocation.
        assert_eq!(
            texts,
            vec![
                "checkout",
                "main",
                "feature-x",
                "hotfix-branch",
                "release-2.1"
            ]
        );

        let completions = history_argument_completions(&entries, "git", &aliases, "release", None);
        assert_eq!(completions.len(), 1);
        assert_eq!(completions[0].text, "release-2.1");
        assert_eq!(completions[0].description.as_deref(), Some("history"));
    }

    #[test]
    fn history_arguments_resolve_aliases_and_keep_quoted_spellings() {
        let entries = vec![crate::history::HistoryEntry {
            command: "g commit -m 'fix: handle | in args'".to_string(),
            timestamp: 0,
            cwd: None,
        }];
        let mut aliases = HashMap::new();
        aliases.insert("g".to_string(), "git".to_string());

        let completions = history_argument_completions(&entries, "git", &aliases, "", None);
        let texts: Vec<&str> = completions.iter().map(|c| c.text.as_str()).collect();
        assert_eq!(texts, vec!["commit", "-m", "'fix: handle | in args'"]);
    }

    #[test]
    fn command_segment_and_word_splitting_respect_quotes() {
        assert_eq!(
            split_command_segments("git log | grep 'a|b' && make > out.txt"),
            vec!["git log ", " grep 'a|b' ", " make ", " out.txt"]
        );
        assert_eq!(
            quote_aware_words(r#"cp "my file" dest\ dir"#),
            vec!["cp", "\"my file\"", r"dest\ dir"]
        );
    }

    #[test]
    fn cd_still_prefers_local_directories() {
        let mut state = ShellState::new(false);
        clear_cache();
        let buffer = "cd sr";
        let (_, completions) = complete(buffer, buffer.len(), &mut state);
        assert!(completions.iter().any(|item| item.text == "src/"));
    }

    #[test]
    fn docker_container_and_image_parsers_build_labelled_completions() {
        let containers = "web\tnginx:1.27\tUp 2 hours\n\
            db\tpostgres:16\tExited (0) 3 days ago\n\
            \t\t\n";
        let results = parse_docker_containers(containers, "");
        let texts: Vec<&str> = results.iter().map(|c| c.text.as_str()).collect();
        assert_eq!(texts, vec!["web", "db"]);
        assert_eq!(
            results[0].description.as_deref(),
            Some("nginx:1.27 — Up 2 hours")
        );
        assert_eq!(parse_docker_containers(containers, "w").len(), 1);

        let images = "nginx:1.27\t68MB\n\
            nginx:latest\t68MB\n\
            myapp:<none>\t120MB\n\
            <none>:<none>\t80MB\n";
        let results = parse_docker_images(images, "");
        let texts: Vec<&str> = results.iter().map(|c| c.text.as_str()).collect();
        assert_eq!(texts, vec!["nginx:1.27", "nginx:latest", "myapp"]);
        assert_eq!(results[0].description.as_deref(), Some("68MB"));
    }

    #[test]
    #[ignore = "requires a running docker daemon"]
    fn docker_probe_lists_live_containers() {
        let results = complete_docker_containers("", false);
        assert!(!results.is_empty());
        for completion in &results {
            assert!(completion.description.is_some());
        }
    }

    #[test]
    fn kill_offers_own_processes_from_proc() {
        let results = complete_system_pids("");
        // The scan sees at least this test process's parent; every entry is a
        // numeric PID that is not this process, labelled with a command name.
        assert!(!results.is_empty());
        let own = std::process::id().to_string();
        for completion in &results {
            assert!(completion.text.parse::<u32>().is_ok());
            assert_ne!(completion.text, own);
            assert!(completion.description.is_some());
        }
    }

    #[test]
    fn package_json_dependencies_complete_for_uninstall() {
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("package.json");
        fs::write(
            &path,
            r#"{
                "dependencies": {"react": "^19.0.0", "left-pad": "1.3.0"},
                "devDependencies": {"typescript": "~5.9.0"}
            }"#,
        )
        .unwrap();

        let results = npm_dependencies_from_path(&path, "");
        let texts: Vec<&str> = results.iter().map(|c| c.text.as_str()).collect();
        assert_eq!(texts, vec!["react", "left-pad", "typescript"]);
        assert_eq!(
            results[0].description.as_deref(),
            Some("dependency ^19.0.0")
        );
        assert_eq!(
            results[2].description.as_deref(),
            Some("dev dependency ~5.9.0")
        );

        let results = npm_dependencies_from_path(&path, "type");
        assert_eq!(results.len(), 1);
    }

    #[test]
    fn sudo_user_option_value_completes_users_until_the_command_starts() {
        assert_eq!(wrapper_value_kind("sudo -u "), Some(WrapperValueKind::User));
        assert_eq!(
            wrapper_value_kind("RUST_LOG=x sudo --user "),
            Some(WrapperValueKind::User)
        );
        assert_eq!(
            wrapper_value_kind("sudo -g "),
            Some(WrapperValueKind::Group)
        );
        assert_eq!(
            wrapper_value_kind("ls | sudo -n -u "),
            Some(WrapperValueKind::User)
        );
        // The wrapped command's own -u is not sudo's.
        assert_eq!(wrapper_value_kind("sudo git -u "), None);
        assert_eq!(wrapper_value_kind("sudo -u root "), None);
        assert_eq!(wrapper_value_kind("chown "), None);

        let mut state = ShellState::new(false);
        clear_cache();
        let buffer = "sudo -u ro";
        let (_, completions) = complete(buffer, buffer.len(), &mut state);
        assert!(completions.iter().any(|item| item.text == "root"));
    }

    #[test]
    fn systemctl_parsers_label_units_and_skip_templates() {
        let units = "ssh.service loaded active running OpenBSD Secure Shell server\n\
            getty@.service loaded inactive dead Getty template\n\
            cron.service loaded active running Regular background program processing daemon\n";
        let results = parse_systemctl_units(units, "");
        let texts: Vec<&str> = results.iter().map(|c| c.text.as_str()).collect();
        assert_eq!(texts, vec!["ssh.service", "cron.service"]);
        assert_eq!(
            results[0].description.as_deref(),
            Some("active — OpenBSD Secure Shell server")
        );
        assert_eq!(parse_systemctl_units(units, "cron").len(), 1);

        let files = "ssh.service enabled enabled\n\
            getty@.service static -\n\
            apport.service disabled disabled\n";
        let results = parse_systemctl_unit_files(files, "");
        let texts: Vec<&str> = results.iter().map(|c| c.text.as_str()).collect();
        assert_eq!(texts, vec!["ssh.service", "apport.service"]);
        assert_eq!(results[0].description.as_deref(), Some("enabled"));
    }

    #[test]
    #[ignore = "requires systemd"]
    fn systemctl_probe_lists_live_units() {
        let results = complete_systemctl_units("", false, false);
        assert!(!results.is_empty());
        let results = complete_systemctl_units("", false, true);
        assert!(!results.is_empty());
    }

    #[test]
    fn user_and_group_completion_orders_human_accounts_first() {
        let passwd = "root:x:0:0:root:/root:/bin/bash\n\
            daemon:x:1:1:daemon:/usr/sbin:/usr/sbin/nologin\n\
            alice:x:1000:1000:Alice:/home/alice:/bin/bash\n\
            nobody:x:65534:65534:nobody:/nonexistent:/usr/sbin/nologin\n";
        let results = parse_passwd_users(passwd, "");
        let texts: Vec<&str> = results.iter().map(|c| c.text.as_str()).collect();
        assert_eq!(texts, vec!["alice", "root", "daemon", "nobody"]);
        assert_eq!(results[0].description.as_deref(), Some("uid 1000"));

        let group = "root:x:0:\n\
            adm:x:4:ubuntu\n\
            alice:x:1000:\n";
        let results = parse_group_entries(group, "a");
        let texts: Vec<&str> = results.iter().map(|c| c.text.as_str()).collect();
        assert_eq!(texts, vec!["alice", "adm"]);

        let mut state = ShellState::new(false);
        clear_cache();
        let buffer = "chown ro";
        let (_, completions) = complete(buffer, buffer.len(), &mut state);
        assert!(completions.iter().any(|item| item.text == "root"));

        clear_cache();
        let buffer = "chown root:";
        let (_, completions) = complete(buffer, buffer.len(), &mut state);
        assert!(completions
            .iter()
            .any(|item| item.text.starts_with("root:")));
    }

    #[test]
    fn trap_completes_signal_and_condition_names() {
        let mut state = ShellState::new(false);
        clear_cache();
        let buffer = "trap 'echo bye' EX";
        let (_, completions) = complete(buffer, buffer.len(), &mut state);
        assert_eq!(completions.len(), 1);
        assert_eq!(completions[0].text, "EXIT");
    }

    #[test]
    fn subcommand_typos_fall_back_to_fuzzy_matches() {
        let mut state = ShellState::new(false);
        clear_cache();
        let buffer = "git chk";
        let (_, completions) = complete(buffer, buffer.len(), &mut state);
        assert!(completions.iter().any(|item| item.text == "checkout"));

        // Prefix matches keep their curated order and exclude fuzzy noise.
        clear_cache();
        let buffer = "git ch";
        let (_, completions) = complete(buffer, buffer.len(), &mut state);
        let texts: Vec<&str> = completions.iter().map(|c| c.text.as_str()).collect();
        assert_eq!(texts, vec!["checkout", "cherry-pick"]);
    }

    #[test]
    fn rank_prefix_then_fuzzy_keeps_prefix_tier_order() {
        let items = vec![
            Completion::new("push".to_string(), CompletionKind::Subcommand),
            Completion::new("pull".to_string(), CompletionKind::Subcommand),
            Completion::new("pop".to_string(), CompletionKind::Subcommand),
        ];
        let ranked = rank_prefix_then_fuzzy(items.clone(), "pu");
        let texts: Vec<&str> = ranked.iter().map(|c| c.text.as_str()).collect();
        assert_eq!(texts, vec!["push", "pull"]);

        let ranked = rank_prefix_then_fuzzy(items, "pp");
        assert_eq!(ranked.len(), 1);
        assert_eq!(ranked[0].text, "pop");
    }

    #[test]
    fn path_completion_falls_back_to_case_insensitive_matches() {
        let tmp = tempfile::tempdir().unwrap();
        fs::create_dir(tmp.path().join("Documents")).unwrap();
        fs::write(tmp.path().join("readme"), "").unwrap();
        fs::write(tmp.path().join("README"), "").unwrap();

        let state = ShellState::new(false);
        let prefix = format!("{}/doc", tmp.path().display());
        let results = complete_path(&prefix, &state);
        assert_eq!(results.len(), 1);
        assert!(results[0].text.ends_with("Documents/"));

        // An exact-case match wins outright; the other casing stays hidden.
        let prefix = format!("{}/read", tmp.path().display());
        let results = complete_path(&prefix, &state);
        assert_eq!(results.len(), 1);
        assert!(results[0].text.ends_with("/readme"));
    }

    #[test]
    fn finalized_lists_deduplicate_by_text_keeping_the_first_source() {
        let completions = vec![
            Completion::new("grep".to_string(), CompletionKind::Command).with_desc("filter lines"),
            Completion::new("grep".to_string(), CompletionKind::Command).with_desc("from PATH"),
            Completion::new("sort".to_string(), CompletionKind::Command),
        ];
        let finalized = finalize_completions(completions);
        assert_eq!(finalized.len(), 2);
        assert_eq!(finalized[0].description.as_deref(), Some("filter lines"));
    }

    #[test]
    fn ssh_hosts_fall_back_to_fuzzy_when_no_prefix_matches() {
        let tmp = tempfile::tempdir().unwrap();
        fs::create_dir_all(tmp.path().join(".ssh")).unwrap();
        fs::write(
            tmp.path().join(".ssh/config"),
            "Host prod-server\nHost dev-box\n",
        )
        .unwrap();

        let hosts = complete_ssh_hosts("prd", false, tmp.path());
        assert_eq!(hosts.len(), 1);
        assert_eq!(hosts[0].text, "prod-server");
    }

    #[test]
    fn parsed_sources_fall_back_to_fuzzy_matches_too() {
        let mut state = ShellState::new(false);
        state
            .env_vars
            .insert("JSH_COMPLETE_ME".to_string(), "value".to_string());
        state
            .aliases
            .insert("gst".to_string(), "git status".to_string());

        // `export JCM` has no prefix match; the subsequence finds the name.
        clear_cache();
        let buffer = "export JCM";
        let (_, completions) = complete(buffer, buffer.len(), &mut state);
        assert!(completions
            .iter()
            .any(|item| item.text == "JSH_COMPLETE_ME"));

        clear_cache();
        let buffer = "unalias gt";
        let (_, completions) = complete(buffer, buffer.len(), &mut state);
        assert!(completions.iter().any(|item| item.text == "gst"));

        // Exact prefixes still exclude fuzzy noise.
        clear_cache();
        let buffer = "export JSH_C";
        let (_, completions) = complete(buffer, buffer.len(), &mut state);
        assert!(completions
            .iter()
            .all(|item| item.text.starts_with("JSH_C")));
    }

    #[test]
    fn probes_run_once_per_command_line_including_their_failures() {
        clear_cache();
        let mut runs = 0;
        for _ in 0..3 {
            let value = probe_text_once("test:source", || {
                runs += 1;
                Some("one\ntwo".to_string())
            });
            assert_eq!(value.as_deref(), Some("one\ntwo"));
        }
        assert_eq!(runs, 1, "a growing prefix must not re-fork the probe");

        // A probe that finds nothing is remembered too: a stopped daemon
        // costs one timeout per command line, not one per keystroke.
        let mut misses = 0;
        for _ in 0..3 {
            let value = probe_text_once("test:absent", || {
                misses += 1;
                None
            });
            assert!(value.is_none());
        }
        assert_eq!(misses, 1);

        // The next command line probes afresh.
        clear_cache();
        let value = probe_text_once("test:source", || Some("three".to_string()));
        assert_eq!(value.as_deref(), Some("three"));
    }

    #[test]
    fn probe_cache_preserves_bytes_that_are_not_utf8() {
        clear_cache();
        let raw = vec![0x66, 0x6f, 0xff, 0x00, 0x6f];
        let cached = probe_once("test:bytes", || Some(raw.clone()));
        assert_eq!(cached.as_deref(), Some(raw.as_slice()));
        // Git's -z status output is parsed as bytes; a lossy round trip
        // through String would rewrite a path this shell must keep exact.
        let again = probe_once("test:bytes", || panic!("must not re-probe"));
        assert_eq!(again.as_deref(), Some(raw.as_slice()));
    }

    #[test]
    fn redirection_targets_are_plain_files_whatever_the_command_is() {
        assert!(is_redirect_target("echo hi >"));
        assert!(is_redirect_target("echo hi >> "));
        assert!(is_redirect_target("wc -l <"));
        assert!(is_redirect_target("cmd 2> "));
        // A descriptor duplication takes a number, not a file.
        assert!(!is_redirect_target("cmd 2>&"));
        // Ordinary argument positions, and operators inside quotes.
        assert!(!is_redirect_target("git add "));
        assert!(!is_redirect_target("echo '>' "));
        assert!(!is_redirect_target("echo \\> "));
        assert!(!is_redirect_target("echo hi > out.txt "));

        let tmp = tempfile::tempdir().unwrap();
        fs::create_dir(tmp.path().join("logs")).unwrap();
        fs::write(tmp.path().join("notes.txt"), "").unwrap();
        let mut state = ShellState::new(false);

        // `cd` normally offers directories only, and `git add` dirty files;
        // after a redirection both must offer the plain file too.
        for command in ["cd", "git add", "echo hi"] {
            clear_cache();
            let buffer = format!("{command} > {}/", tmp.path().display());
            let (_, completions) = complete(&buffer, buffer.len(), &mut state);
            let texts: Vec<&str> = completions.iter().map(|c| c.text.as_str()).collect();
            assert!(
                texts.iter().any(|t| t.ends_with("notes.txt")),
                "{command}: {texts:?}"
            );
        }
    }

    #[test]
    fn completing_inside_quotes_keeps_the_quoting_style() {
        let tmp = tempfile::tempdir().unwrap();
        fs::write(tmp.path().join("my file.txt"), "").unwrap();
        fs::create_dir(tmp.path().join("my dir")).unwrap();

        assert_eq!(open_quote_context("\"my fi"), Some(('"', "my fi")));
        assert_eq!(open_quote_context("'my fi"), Some(('\'', "my fi")));
        // Already-closed quotes are ordinary words again.
        assert_eq!(open_quote_context("\"done\""), None);
        assert_eq!(open_quote_context("plain"), None);

        let mut state = ShellState::new(false);
        clear_cache();
        let buffer = format!("cat \"{}/my fi", tmp.path().display());
        let (_, completions) = complete(&buffer, buffer.len(), &mut state);
        let file = completions
            .iter()
            .find(|c| c.text.ends_with("my file.txt\""))
            .expect("file candidate keeps its quotes closed");
        assert!(file.text.starts_with('"'));
        assert!(
            !file.text.contains('\\'),
            "no backslash escaping inside quotes"
        );

        clear_cache();
        let buffer = format!("cat \"{}/my d", tmp.path().display());
        let (_, completions) = complete(&buffer, buffer.len(), &mut state);
        let dir = completions
            .iter()
            .find(|c| c.text.ends_with("my dir/"))
            .expect("directory candidate");
        // The path continues, so the quote stays open for the next segment.
        assert_eq!(dir.text.matches('"').count(), 1);
    }

    #[test]
    fn jsh_builtins_complete_their_own_arguments() {
        let mut state = ShellState::new(false);

        clear_cache();
        let buffer = "shopt -s globs";
        let (_, completions) = complete(buffer, buffer.len(), &mut state);
        assert!(completions.iter().any(|item| item.text == "globstar"));

        clear_cache();
        let buffer = "set -o pipe";
        let (_, completions) = complete(buffer, buffer.len(), &mut state);
        let option = completions
            .iter()
            .find(|item| item.text == "pipefail")
            .unwrap();
        assert!(option.description.is_none() || !option.description.as_ref().unwrap().is_empty());

        clear_cache();
        let buffer = "set -o err";
        let (_, completions) = complete(buffer, buffer.len(), &mut state);
        let errexit = completions
            .iter()
            .find(|item| item.text == "errexit")
            .unwrap();
        assert_eq!(errexit.description.as_deref(), Some("same as -e"));

        clear_cache();
        let buffer = "hook add ch";
        let (_, completions) = complete(buffer, buffer.len(), &mut state);
        assert!(completions.iter().any(|item| item.text == "chpwd"));
    }

    #[test]
    fn hook_remove_offers_only_registered_hooks() {
        let mut state = ShellState::new(false);
        state.hooks.precmd.push("update_title".to_string());
        state.functions.insert(
            "unrelated_function".to_string(),
            crate::parser::ast::CompoundCommand::BraceGroup {
                body: Vec::new(),
                redirects: Vec::new(),
            },
        );

        clear_cache();
        let buffer = "hook remove precmd ";
        let (_, completions) = complete(buffer, buffer.len(), &mut state);
        let texts: Vec<&str> = completions.iter().map(|c| c.text.as_str()).collect();
        assert_eq!(texts, vec!["update_title"]);

        // `hook add` names any function, since none is registered yet.
        clear_cache();
        let buffer = "hook add precmd ";
        let (_, completions) = complete(buffer, buffer.len(), &mut state);
        assert!(completions
            .iter()
            .any(|item| item.text == "unrelated_function"));
    }

    #[test]
    fn commands_that_open_one_kind_of_file_offer_that_kind() {
        let tmp = tempfile::tempdir().unwrap();
        fs::write(tmp.path().join("setup.sh"), "").unwrap();
        fs::write(tmp.path().join("notes.txt"), "").unwrap();
        fs::write(tmp.path().join("app.py"), "").unwrap();
        fs::create_dir(tmp.path().join("scripts")).unwrap();
        let mut state = ShellState::new(false);

        clear_cache();
        let buffer = format!("source {}/", tmp.path().display());
        let (_, completions) = complete(&buffer, buffer.len(), &mut state);
        let names: Vec<&str> = completions.iter().map(|c| c.display.as_str()).collect();
        assert!(names.contains(&"setup.sh"));
        assert!(names.contains(&"scripts/"), "directories lead to the file");
        assert!(!names.contains(&"notes.txt"));
        assert!(!names.contains(&"app.py"));

        clear_cache();
        let buffer = format!("python {}/", tmp.path().display());
        let (_, completions) = complete(&buffer, buffer.len(), &mut state);
        let names: Vec<&str> = completions.iter().map(|c| c.display.as_str()).collect();
        assert!(names.contains(&"app.py"));
        assert!(!names.contains(&"setup.sh"));

        // A directory holding nothing of that kind keeps the whole listing
        // rather than showing an empty menu.
        let plain = tempfile::tempdir().unwrap();
        fs::write(plain.path().join("notes.txt"), "").unwrap();
        clear_cache();
        let buffer = format!("source {}/", plain.path().display());
        let (_, completions) = complete(&buffer, buffer.len(), &mut state);
        assert!(completions.iter().any(|c| c.display == "notes.txt"));

        // An unlisted command is untouched, and so are compressors, whose
        // argument is whatever file is being compressed.
        clear_cache();
        let buffer = format!("gzip {}/", tmp.path().display());
        let (_, completions) = complete(&buffer, buffer.len(), &mut state);
        assert!(completions.iter().any(|c| c.display == "notes.txt"));
    }

    #[test]
    fn ssh_config_includes_are_followed_and_cycles_terminate() {
        let tmp = tempfile::tempdir().unwrap();
        let ssh = tmp.path().join(".ssh");
        fs::create_dir_all(ssh.join("config.d")).unwrap();
        fs::write(
            ssh.join("config"),
            "Include config.d/*.conf\nInclude ~/.ssh/extra\nHost direct\n",
        )
        .unwrap();
        fs::write(ssh.join("config.d/work.conf"), "Host workbox\n").unwrap();
        fs::write(ssh.join("config.d/home.conf"), "Host homelab\n").unwrap();
        // Not matched by the glob, so never read.
        fs::write(ssh.join("config.d/notes.txt"), "Host ignored\n").unwrap();
        // An include cycle must terminate rather than recurse forever.
        fs::write(ssh.join("extra"), "Host extrabox\nInclude config\n").unwrap();

        let hosts = complete_ssh_hosts("", false, tmp.path());
        let texts: Vec<&str> = hosts.iter().map(|c| c.text.as_str()).collect();
        assert!(texts.contains(&"direct"));
        assert!(texts.contains(&"workbox"));
        assert!(texts.contains(&"homelab"));
        assert!(texts.contains(&"extrabox"));
        assert!(!texts.contains(&"ignored"));
    }

    #[test]
    fn ssh_config_match_blocks_are_not_host_aliases() {
        let (hosts, includes) = parse_ssh_config_directives(
            "Match host bastion\n  ProxyJump none\nHost real\nInclude other/*\n",
        );
        assert_eq!(hosts, vec!["real"]);
        assert_eq!(includes, vec!["other/*"]);
    }

    #[test]
    fn compose_services_come_from_the_project_file_not_the_daemon() {
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("compose.yaml");
        fs::write(
            &path,
            "services:\n\
             \x20 web:\n\
             \x20   image: nginx:1.27\n\
             \x20 worker:\n\
             \x20   build:\n\
             \x20     context: ./worker\n\
             \x20 plain:\n\
             \x20   command: sleep 1\n\
             volumes:\n\
             \x20 data:\n",
        )
        .unwrap();

        let services = compose_services_from_path(&path, "");
        let texts: Vec<&str> = services.iter().map(|c| c.text.as_str()).collect();
        assert_eq!(texts, vec!["web", "worker", "plain"]);
        assert_eq!(services[0].description.as_deref(), Some("nginx:1.27"));
        assert_eq!(services[1].description.as_deref(), Some("build ./worker"));
        assert_eq!(services[2].description.as_deref(), Some("compose service"));

        assert_eq!(compose_services_from_path(&path, "wo").len(), 1);
        // A file without services, and a file that is not YAML at all.
        let empty = tmp.path().join("empty.yaml");
        fs::write(&empty, "version: '3'\n").unwrap();
        assert!(compose_services_from_path(&empty, "").is_empty());
    }

    #[test]
    fn kubeconfig_names_come_from_the_file_and_mark_the_current_context() {
        let config = "apiVersion: v1\n\
            current-context: prod\n\
            clusters:\n\
            - name: prod-cluster\n\
            - name: dev-cluster\n\
            users:\n\
            - name: admin\n\
            contexts:\n\
            - name: prod\n\
            \x20 context:\n\
            \x20   cluster: prod-cluster\n\
            \x20   namespace: payments\n\
            - name: dev\n\
            \x20 context:\n\
            \x20   cluster: dev-cluster\n\
            \x20   namespace: default\n";

        let contexts = parse_kubeconfig_names(config, KubeName::Context, "");
        let texts: Vec<&str> = contexts.iter().map(|c| c.text.as_str()).collect();
        assert_eq!(texts, vec!["prod", "dev"]);
        assert_eq!(contexts[0].description.as_deref(), Some("current context"));
        assert_eq!(contexts[1].description.as_deref(), Some("context"));

        let namespaces = parse_kubeconfig_names(config, KubeName::Namespace, "");
        let texts: Vec<&str> = namespaces.iter().map(|c| c.text.as_str()).collect();
        assert_eq!(texts, vec!["payments", "default"]);
        assert_eq!(
            namespaces[0].description.as_deref(),
            Some("default for prod")
        );

        assert_eq!(
            parse_kubeconfig_names(config, KubeName::Cluster, "dev")
                .first()
                .map(|c| c.text.as_str()),
            Some("dev-cluster")
        );
        assert_eq!(parse_kubeconfig_names(config, KubeName::User, "").len(), 1);
        assert!(parse_kubeconfig_names("not: [valid", KubeName::Context, "").is_empty());
    }

    #[test]
    fn kubectl_completes_contexts_and_namespaces_from_the_home_kubeconfig() {
        let tmp = tempfile::tempdir().unwrap();
        fs::create_dir_all(tmp.path().join(".kube")).unwrap();
        fs::write(
            tmp.path().join(".kube/config"),
            "current-context: prod\n\
             contexts:\n\
             - name: prod\n\
             \x20 context:\n\
             \x20   namespace: payments\n",
        )
        .unwrap();

        let mut state = ShellState::new(false);
        state.home_dir = tmp.path().to_path_buf();

        clear_cache();
        let buffer = "kubectl -n pay";
        let (_, completions) = complete(buffer, buffer.len(), &mut state);
        assert!(completions.iter().any(|item| item.text == "payments"));

        clear_cache();
        let buffer = "kubectl config use-context pr";
        let (_, completions) = complete(buffer, buffer.len(), &mut state);
        assert!(completions.iter().any(|item| item.text == "prod"));

        // The inline flag form keeps its prefix on the inserted text.
        clear_cache();
        let buffer = "kubectl get pods --context=pr";
        let (_, completions) = complete(buffer, buffer.len(), &mut state);
        assert!(completions.iter().any(|item| item.text == "--context=prod"));

        // KUBECONFIG in this shell's own environment wins over ~/.kube.
        let other = tempfile::tempdir().unwrap();
        let elsewhere = other.path().join("other.yaml");
        fs::write(&elsewhere, "contexts:\n- name: staging\n").unwrap();
        state
            .env_vars
            .insert("KUBECONFIG".to_string(), elsewhere.display().to_string());

        clear_cache();
        let buffer = "kubectx sta";
        let (_, completions) = complete(buffer, buffer.len(), &mut state);
        let texts: Vec<&str> = completions.iter().map(|c| c.text.as_str()).collect();
        assert_eq!(texts, vec!["staging"]);
    }

    #[test]
    fn tilde_user_and_variable_prefixes_resolve_for_the_scan_only() {
        let mut state = ShellState::new(false);
        let tmp = tempfile::tempdir().unwrap();
        fs::create_dir(tmp.path().join("projects")).unwrap();
        fs::write(tmp.path().join("notes.txt"), "").unwrap();
        state
            .env_vars
            .insert("JSH_BASE".to_string(), tmp.path().display().to_string());

        // `$VAR/` scans the value but the candidate keeps the variable.
        clear_cache();
        let buffer = "ls $JSH_BASE/pro";
        let (_, completions) = complete(buffer, buffer.len(), &mut state);
        assert!(completions
            .iter()
            .any(|item| item.text == "$JSH_BASE/projects/"));

        // The braced spelling resolves the same way.
        clear_cache();
        let buffer = "ls ${JSH_BASE}/not";
        let (_, completions) = complete(buffer, buffer.len(), &mut state);
        assert!(completions
            .iter()
            .any(|item| item.text == "${JSH_BASE}/notes.txt"));

        // A trailing slash is required: `$JSH_BAS` is still a variable name
        // being typed, and completes as one.
        assert_eq!(
            expand_completion_prefix("$JSH_BASE", &state),
            None,
            "a bare variable is not a directory to scan"
        );
        assert_eq!(expand_completion_prefix("$UNSET_VARIABLE/x", &state), None);

        // `~` keeps working, and `~user/` resolves through the user database.
        state.home_dir = tmp.path().to_path_buf();
        assert_eq!(
            expand_completion_prefix("~/pro", &state).as_deref(),
            Some(format!("{}/pro", tmp.path().display()).as_str())
        );
        let root_home = passwd_home_dir("root");
        assert!(root_home.is_some(), "root is in the user database");
        assert_eq!(
            expand_completion_prefix("~root/", &state),
            root_home.map(|home| format!("{}/", home.trim_end_matches('/')))
        );
        assert_eq!(expand_completion_prefix("~nosuchuser/x", &state), None);
    }

    #[test]
    fn tilde_completes_user_names_as_home_directories() {
        let homes = complete_user_homes("roo");
        let root = homes
            .iter()
            .find(|completion| completion.text == "~root/")
            .expect("root completes as a home directory");
        assert_eq!(root.display, "~root");
        assert!(root.is_dir, "accepting it continues into the directory");

        let mut state = ShellState::new(false);
        clear_cache();
        let buffer = "ls ~roo";
        let (_, completions) = complete(buffer, buffer.len(), &mut state);
        assert!(completions.iter().any(|item| item.text == "~root/"));
    }

    #[test]
    fn worktree_listing_pairs_paths_with_what_is_checked_out() {
        let output = "worktree /home/u/proj\n\
            HEAD abc123\n\
            branch refs/heads/master\n\
            \n\
            worktree /home/u/proj-fix\n\
            HEAD def456\n\
            detached\n\
            \n\
            worktree /home/u/other\n\
            HEAD 000\n\
            branch refs/heads/feature\n";
        let results = parse_git_worktrees(output, "");
        let texts: Vec<&str> = results.iter().map(|c| c.text.as_str()).collect();
        assert_eq!(
            texts,
            vec!["/home/u/proj", "/home/u/proj-fix", "/home/u/other"]
        );
        assert_eq!(results[0].description.as_deref(), Some("master"));
        assert_eq!(results[1].description.as_deref(), Some("detached HEAD"));

        assert_eq!(parse_git_worktrees(output, "/home/u/proj-").len(), 1);
    }

    #[test]
    fn deleting_a_branch_never_offers_the_one_checked_out() {
        // Runs in this repository, so the probe has real branches. Whatever
        // the current branch is, it must not be offered for deletion.
        let mut state = ShellState::new(false);
        state.cached_git_branch = crate::prompt::probe_git_context().branch;
        let Some(current) = state.cached_git_branch.clone() else {
            return; // Not in a repository; nothing to assert.
        };

        clear_cache();
        let buffer = "git branch -d ";
        let (_, completions) = complete(buffer, buffer.len(), &mut state);
        assert!(
            completions.iter().all(|item| item.text != current),
            "the checked-out branch cannot be deleted"
        );
        // Tags and remotes are not branches either.
        assert!(completions
            .iter()
            .all(|item| !item.text.contains("origin/")));
    }

    #[test]
    fn git_config_offers_well_known_keys_beside_the_ones_already_set() {
        let mut state = ShellState::new(false);
        clear_cache();
        let buffer = "git config user.";
        let (_, completions) = complete(buffer, buffer.len(), &mut state);
        let texts: Vec<&str> = completions.iter().map(|c| c.text.as_str()).collect();
        assert!(texts.contains(&"user.email"), "{texts:?}");
        assert!(texts.contains(&"user.name"), "{texts:?}");

        clear_cache();
        let buffer = "git config init.def";
        let (_, completions) = complete(buffer, buffer.len(), &mut state);
        let key = completions
            .iter()
            .find(|item| item.text == "init.defaultBranch")
            .unwrap();
        assert!(key.description.is_some());
    }

    #[test]
    fn everyday_commands_explain_their_flags() {
        let mut state = ShellState::new(false);

        clear_cache();
        let buffer = "curl -";
        let (_, completions) = complete(buffer, buffer.len(), &mut state);
        let follow = completions.iter().find(|item| item.text == "-L").unwrap();
        assert_eq!(follow.description.as_deref(), Some("follow redirects"));

        clear_cache();
        let buffer = "tail -f";
        let (_, completions) = complete(buffer, buffer.len(), &mut state);
        assert_eq!(
            completions
                .iter()
                .find(|item| item.text == "-f")
                .and_then(|item| item.description.as_deref()),
            Some("follow as it grows")
        );

        // Long options are offered too, and every table entry is a flag.
        clear_cache();
        let buffer = "rsync --del";
        let (_, completions) = complete(buffer, buffer.len(), &mut state);
        assert!(completions.iter().any(|item| item.text == "--delete"));
        assert!(completions
            .iter()
            .all(|item| item.kind == CompletionKind::Flag));

        // A command with no table still falls through to whatever else the
        // completer knows, rather than answering nothing.
        clear_cache();
        let buffer = "git -";
        let (_, completions) = complete(buffer, buffer.len(), &mut state);
        assert!(!completions.is_empty());
    }

    #[test]
    fn flag_tables_are_well_formed() {
        for (command, flags) in COMMON_FLAGS {
            assert!(!flags.is_empty(), "{command} has no flags");
            let mut seen = std::collections::HashSet::new();
            for (flag, description) in *flags {
                assert!(seen.insert(*flag), "{command} lists {flag} twice");
                assert!(!description.is_empty(), "{command} {flag} has no help");
                // `ps aux` is the one bare-word idiom people type as a flag.
                assert!(
                    flag.starts_with('-') || (*command == "ps" && *flag == "aux"),
                    "{command} {flag} is not a flag"
                );
            }
        }
    }

    #[test]
    fn spec_generators_resolve_to_builtin_sources_and_never_to_commands() {
        let mut state = ShellState::new(false);
        state
            .env_vars
            .insert("JSH_GENERATOR_VAR".to_string(), "x".to_string());

        // A generator name reaches the same dynamic source the built-in
        // branches use.
        let results = complete_from_generator("environment_variables", "JSH_GENER", &state);
        assert!(results.iter().any(|item| item.text == "JSH_GENERATOR_VAR"));

        let signals = complete_from_generator("signals", "TE", &state);
        assert!(signals.iter().any(|item| item.text == "TERM"));

        // An unknown name yields nothing rather than anything surprising,
        // and a name that looks like a command is just an unknown name.
        assert!(complete_from_generator("not_a_generator", "", &state).is_empty());
        assert!(complete_from_generator("bash -c 'echo pwned'", "", &state).is_empty());
        assert!(complete_from_generator("$(id)", "", &state).is_empty());
    }

    #[test]
    fn generator_names_in_shipped_specs_all_resolve() {
        // A generator in a shipped spec that no longer exists would silently
        // complete nothing; every name must still be one this build knows.
        let state = ShellState::new(false);
        let known = [
            "git_refs",
            "git_branches",
            "git_tags",
            "git_remotes",
            "git_worktrees",
            "git_config_keys",
            "git_modified_files",
            "ssh_hosts",
            "users",
            "groups",
            "processes",
            "signals",
            "docker_containers",
            "docker_running_containers",
            "docker_images",
            "compose_services",
            "systemd_units",
            "systemd_unit_files",
            "kube_contexts",
            "kube_namespaces",
            "npm_scripts",
            "npm_dependencies",
            "make_targets",
            "cargo_bins",
            "cargo_features",
            "cargo_packages",
            "environment_variables",
            "shell_functions",
            "bookmarks",
        ];
        // Each documented name is routed: an unrouted one would fall to the
        // catch-all, which is what `not_a_generator` proves returns nothing.
        for name in known {
            let _ = complete_from_generator(name, "\u{1}unmatchable", &state);
        }

        let registry = crate::completion_spec::SpecRegistry::new();
        let spec = registry.get("git").expect("git spec ships with the shell");
        let generators: Vec<String> = spec
            .subcommands
            .iter()
            .flat_map(|sub| sub.args.iter())
            .filter_map(|arg| match &arg.template {
                crate::completion_spec::ArgTemplate::Generator(name) => Some(name.clone()),
                _ => None,
            })
            .collect();
        assert!(
            !generators.is_empty(),
            "the git spec uses generators for its ref arguments"
        );
        for generator in generators {
            assert!(
                known.contains(&generator.as_str()),
                "{generator} is not a generator this build resolves"
            );
        }
    }

    #[test]
    fn version_managers_offer_what_is_installed_newest_first() {
        let tmp = tempfile::tempdir().unwrap();
        let versions = tmp.path().join(".nvm/versions/node");
        for version in ["v9.1.0", "v20.11.0", "v18.19.1"] {
            fs::create_dir_all(versions.join(version)).unwrap();
        }
        // A stray file among the version directories is not a version.
        fs::write(versions.join("README"), "").unwrap();

        let mut state = ShellState::new(false);
        state.home_dir = tmp.path().to_path_buf();
        // The machine running the tests may have its own nvm; this asserts
        // about the one in the temporary home.
        state.env_vars.remove("NVM_DIR");

        let results = complete_toolchain_versions("nvm", &state);
        let texts: Vec<&str> = results.iter().map(|c| c.text.as_str()).collect();
        assert_eq!(texts, vec!["v20.11.0", "v18.19.1", "v9.1.0"]);
        assert_eq!(results[0].description.as_deref(), Some("installed node"));

        clear_cache();
        let buffer = "nvm use v18";
        let (_, completions) = complete(buffer, buffer.len(), &mut state);
        assert!(completions.iter().any(|item| item.text == "v18.19.1"));

        // NVM_DIR from this shell's environment relocates the search.
        let other = tempfile::tempdir().unwrap();
        fs::create_dir_all(other.path().join("versions/node/v22.0.0")).unwrap();
        state
            .env_vars
            .insert("NVM_DIR".to_string(), other.path().display().to_string());
        let results = complete_toolchain_versions("nvm", &state);
        let texts: Vec<&str> = results.iter().map(|c| c.text.as_str()).collect();
        assert_eq!(texts, vec!["v22.0.0"]);

        assert_eq!(
            natural_version_order("v20.11.0", "v9.1.0"),
            std::cmp::Ordering::Greater
        );
    }

    #[test]
    fn source_finds_the_virtual_environment_activate_script() {
        let tmp = tempfile::tempdir().unwrap();
        fs::create_dir_all(tmp.path().join(".venv/bin")).unwrap();
        fs::write(tmp.path().join(".venv/bin/activate"), "").unwrap();
        fs::write(tmp.path().join(".venv/bin/python"), "").unwrap();

        let mut state = ShellState::new(false);
        state.env_vars.remove("VIRTUAL_ENV");
        let found = venv_activators_in(tmp.path(), &state);
        assert_eq!(found.len(), 1);
        assert_eq!(found[0].text, ".venv/bin/activate");
        assert_eq!(found[0].description.as_deref(), Some("activate .venv"));

        // A directory with no environment offers nothing of its own.
        let bare = tempfile::tempdir().unwrap();
        assert!(venv_activators_in(bare.path(), &state).is_empty());

        // An environment named in this shell's own variables is offered by
        // its full path, wherever it lives.
        let elsewhere = tempfile::tempdir().unwrap();
        fs::create_dir_all(elsewhere.path().join("bin")).unwrap();
        fs::write(elsewhere.path().join("bin/activate"), "").unwrap();
        state.env_vars.insert(
            "VIRTUAL_ENV".to_string(),
            elsewhere.path().display().to_string(),
        );
        let found = venv_activators_in(bare.path(), &state);
        assert_eq!(found.len(), 1);
        assert_eq!(
            found[0].description.as_deref(),
            Some("the active environment")
        );
        assert!(found[0].text.ends_with("/bin/activate"));
    }

    #[test]
    fn commands_are_recognised_after_keywords_and_inside_substitutions() {
        let mut state = ShellState::new(false);

        // A command follows `do`, `then` and the rest, not an argument.
        for buffer in [
            "while read line; do ec",
            "if true; then ec",
            "if true; then :; else ec",
            "for f in *; do ec",
            "if ec",
            "! ec",
        ] {
            clear_cache();
            let (_, completions) = complete(buffer, buffer.len(), &mut state);
            assert!(
                completions.iter().any(|item| item.text == "echo"),
                "{buffer}"
            );
        }

        // Backtick substitution starts a command, like `$(` already did.
        clear_cache();
        let buffer = "echo `git pu";
        let (_, completions) = complete(buffer, buffer.len(), &mut state);
        assert!(completions.iter().any(|item| item.text == "push"));

        clear_cache();
        let buffer = "echo $(git pu";
        let (_, completions) = complete(buffer, buffer.len(), &mut state);
        assert!(completions.iter().any(|item| item.text == "push"));

        // The keyword itself is still a command name while being typed.
        clear_cache();
        let buffer = "whil";
        let (_, completions) = complete(buffer, buffer.len(), &mut state);
        assert!(completions.iter().any(|item| item.text == "while"));
    }

    #[test]
    fn completion_uses_the_text_before_the_cursor_not_the_whole_line() {
        let mut state = ShellState::new(false);
        // The cursor sits after `pu`, with an old argument still to its
        // right: the completion is for the word being edited.
        let buffer = "git pu --force";
        let cursor = "git pu".len();
        clear_cache();
        let (word_start, completions) = complete(buffer, cursor, &mut state);
        assert_eq!(word_start, 4);
        assert!(completions.iter().any(|item| item.text == "push"));

        // A cursor inside an earlier command of a pipeline completes that
        // command, not the one after the pipe.
        let buffer = "car | grep x";
        let cursor = "car".len();
        clear_cache();
        let (word_start, completions) = complete(buffer, cursor, &mut state);
        assert_eq!(word_start, 0);
        assert!(completions.iter().any(|item| item.text == "cargo"));
    }

    #[test]
    fn multi_word_aliases_complete_as_the_command_they_stand_for() {
        let mut state = ShellState::new(false);
        state
            .aliases
            .insert("gs".to_string(), "git status".to_string());
        state
            .aliases
            .insert("gco".to_string(), "git checkout".to_string());
        state
            .aliases
            .insert("dc".to_string(), "docker compose".to_string());

        // `gc --am` is a `git commit` flag position, from the git spec.
        state
            .aliases
            .insert("gc".to_string(), "git commit".to_string());
        clear_cache();
        let buffer = "gc --am";
        let (_, completions) = complete(buffer, buffer.len(), &mut state);
        assert!(
            completions.iter().any(|item| item.text == "--amend"),
            "expected git commit flags"
        );

        // And its arguments: `gco ` reaches ref completion.
        clear_cache();
        let buffer = "gco ";
        let (_, completions) = complete(buffer, buffer.len(), &mut state);
        assert!(
            completions
                .iter()
                .any(|item| item.kind == CompletionKind::Other),
            "expected git checkout refs"
        );

        // `dc ` reaches the compose subcommands, two words in.
        clear_cache();
        let buffer = "dc lo";
        let (_, completions) = complete(buffer, buffer.len(), &mut state);
        assert!(completions.iter().any(|item| item.text == "logs"));

        // The expansion is only the head word: an argument already typed
        // still counts, so `gco` reaches ref completion rather than the
        // subcommand list.
        assert_eq!(
            alias_expanded_segment("gco ", &state.aliases),
            "git checkout "
        );
        assert_eq!(
            alias_expanded_segment("sudo gs ", &state.aliases),
            "sudo git status "
        );
    }

    #[test]
    fn alias_expansion_terminates_on_self_reference_and_cycles() {
        let mut aliases = HashMap::new();
        // The idiomatic self-referential alias must expand exactly once.
        aliases.insert("ls".to_string(), "ls --color=auto".to_string());
        assert_eq!(alias_expanded_segment("ls ", &aliases), "ls --color=auto ");

        // A cycle between two aliases must not spin.
        aliases.insert("a".to_string(), "b x".to_string());
        aliases.insert("b".to_string(), "a y".to_string());
        let expanded = alias_expanded_segment("a ", &aliases);
        assert!(expanded.contains('x') || expanded.contains('y'));

        // An unknown head word is left alone.
        assert_eq!(alias_expanded_segment("git st ", &aliases), "git st ");
    }

    #[test]
    fn options_with_a_fixed_set_of_values_offer_them() {
        let mut state = ShellState::new(false);

        // Separate form.
        clear_cache();
        let buffer = "curl -X PO";
        let (_, completions) = complete(buffer, buffer.len(), &mut state);
        let post = completions.iter().find(|item| item.text == "POST").unwrap();
        assert_eq!(post.description.as_deref(), Some("create or submit"));

        // Inline form keeps the option on the inserted text.
        clear_cache();
        let buffer = "git log --pretty=one";
        let (_, completions) = complete(buffer, buffer.len(), &mut state);
        assert!(completions
            .iter()
            .any(|item| item.text == "--pretty=oneline"));

        clear_cache();
        let buffer = "find . -type ";
        let (_, completions) = complete(buffer, buffer.len(), &mut state);
        let texts: Vec<&str> = completions.iter().map(|c| c.text.as_str()).collect();
        assert!(texts.contains(&"d"), "{texts:?}");
        assert!(texts.contains(&"f"), "{texts:?}");

        clear_cache();
        let buffer = "journalctl -p err";
        let (_, completions) = complete(buffer, buffer.len(), &mut state);
        assert!(completions.iter().any(|item| item.text == "err"));

        // An option with no fixed set is untouched: `-o` for curl is a file.
        assert!(option_value_choices("curl", "-o").is_none());
        assert!(option_value_choices("unknown", "-X").is_none());
    }

    #[test]
    fn option_value_tables_are_well_formed() {
        // Every documented pair must be reachable and described.
        for (cmd, option) in [
            ("curl", "-X"),
            ("git", "--pretty"),
            ("systemctl", "--state"),
            ("journalctl", "-o"),
            ("kubectl", "-o"),
            ("find", "-type"),
            ("ps", "-o"),
            ("docker", "--restart"),
        ] {
            let values = option_value_choices(cmd, option)
                .unwrap_or_else(|| panic!("{cmd} {option} has no values"));
            let mut seen = std::collections::HashSet::new();
            for (value, description) in values {
                assert!(seen.insert(*value), "{cmd} {option} repeats {value}");
                assert!(
                    !description.is_empty(),
                    "{cmd} {option} {value} has no help"
                );
                assert!(!value.is_empty());
            }
        }
    }

    #[test]
    fn structured_fields_are_read_from_each_format_with_their_types() {
        let json = r#"[{"id": 1, "name": "ada", "score": 9.5, "tags": ["x"], "ok": true}]"#;
        let fields = structured_fields(json, "json").unwrap();
        assert_eq!(
            fields,
            vec![
                ("id".to_string(), "int".to_string()),
                ("name".to_string(), "string".to_string()),
                ("score".to_string(), "float".to_string()),
                ("tags".to_string(), "list".to_string()),
                ("ok".to_string(), "bool".to_string()),
            ]
        );

        // A bare record, not only a list of them.
        let fields = structured_fields(r#"{"host": "a"}"#, "json").unwrap();
        assert_eq!(fields, vec![("host".to_string(), "string".to_string())]);

        // NDJSON is typed from its first line.
        let fields = structured_fields("\n{\"a\": 1}\n{\"b\": 2}\n", "ndjson").unwrap();
        assert_eq!(fields, vec![("a".to_string(), "int".to_string())]);

        // CSV names fields in the header and types them from the first row.
        let fields = structured_fields("name,count,ratio\nada,3,0.5\n", "csv").unwrap();
        assert_eq!(
            fields,
            vec![
                ("name".to_string(), "string".to_string()),
                ("count".to_string(), "int".to_string()),
                ("ratio".to_string(), "float".to_string()),
            ]
        );
        // A header with no data row still names the fields.
        let fields = structured_fields("name,count\n", "csv").unwrap();
        assert_eq!(fields[0], ("name".to_string(), "column".to_string()));

        let fields = structured_fields("host: a\nport: 8080\n", "yaml").unwrap();
        assert_eq!(
            fields,
            vec![
                ("host".to_string(), "string".to_string()),
                ("port".to_string(), "int".to_string()),
            ]
        );

        let fields = structured_fields("name = \"jsh\"\nedition = 2021\n", "toml").unwrap();
        assert!(fields.contains(&("name".to_string(), "string".to_string())));

        // Malformed input and unknown formats yield nothing at all.
        assert!(structured_fields("{not json", "json").is_none());
        assert!(structured_fields("anything", "xlsx").is_none());
    }

    #[test]
    fn value_builtins_complete_field_names_from_the_pipeline_source() {
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("orders.json");
        fs::write(
            &path,
            r#"[{"order_id": 7, "customer": "ada", "total": 12.5}]"#,
        )
        .unwrap();
        let mut state = ShellState::new(false);

        clear_cache();
        let buffer = format!("from-json {} | where cust", path.display());
        let (_, completions) = complete(&buffer, buffer.len(), &mut state);
        let field = completions
            .iter()
            .find(|item| item.text == "customer")
            .expect("field from the source file");
        assert_eq!(field.description.as_deref(), Some("string"));

        // Every field-taking builtin sees them, and only its first argument.
        clear_cache();
        let buffer = format!("from-json {} | sort-by tot", path.display());
        let (_, completions) = complete(&buffer, buffer.len(), &mut state);
        assert!(completions.iter().any(|item| item.text == "total"));

        clear_cache();
        let buffer = format!("from-json {} | where total > cust", path.display());
        let (_, completions) = complete(&buffer, buffer.len(), &mut state);
        assert!(
            !completions.iter().any(|item| item.text == "customer"),
            "only the first argument names a field"
        );

        // A pipeline with no structured source falls through to the usual
        // completions rather than inventing fields.
        clear_cache();
        let buffer = "ps aux | where cust";
        let (_, completions) = complete(buffer, buffer.len(), &mut state);
        assert!(!completions.iter().any(|item| item.text == "customer"));
    }

    #[test]
    fn the_command_candidate_cache_notices_new_names() {
        let mut state = ShellState::new(false);
        clear_cache();
        let before = command_candidates(&mut state);
        assert!(!before
            .iter()
            .any(|item| item.text == "jsh_cached_alias_probe"));

        // A name defined without an intervening command still shows up: the
        // generation check, not only `clear_cache`, has to catch it.
        state
            .aliases
            .insert("jsh_cached_alias_probe".to_string(), "echo hi".to_string());
        let after = command_candidates(&mut state);
        assert!(
            after
                .iter()
                .any(|item| item.text == "jsh_cached_alias_probe"),
            "a new alias invalidates the candidate cache"
        );

        // Functions are counted separately, so they invalidate it too.
        state.functions.insert(
            "jsh_cached_fn_probe".to_string(),
            crate::parser::ast::CompoundCommand::BraceGroup {
                body: Vec::new(),
                redirects: Vec::new(),
            },
        );
        let after = command_candidates(&mut state);
        assert!(after.iter().any(|item| item.text == "jsh_cached_fn_probe"));

        // An unchanged shell hands back the very same list, not a rebuild.
        let again = command_candidates(&mut state);
        assert!(std::rc::Rc::ptr_eq(&after, &again));
    }

    #[test]
    fn command_completion_still_ranks_and_deduplicates() {
        let mut state = ShellState::new(false);
        clear_cache();
        let buffer = "ech";
        let (_, completions) = complete(buffer, buffer.len(), &mut state);
        assert_eq!(
            completions.first().map(|item| item.text.as_str()),
            Some("echo"),
            "the exact prefix match ranks first"
        );
        let mut seen = std::collections::HashSet::new();
        for completion in &completions {
            assert!(
                seen.insert(completion.text.clone()),
                "{} appears twice",
                completion.text
            );
        }
        assert!(completions.len() <= 50, "the list stays bounded");
    }

    #[test]
    fn accepted_candidates_lead_without_changing_the_list() {
        let items = |texts: &[&str]| -> Vec<Completion> {
            texts
                .iter()
                .map(|text| Completion::new((*text).to_string(), CompletionKind::Other))
                .collect()
        };

        // With nothing recorded the order is exactly the source's order.
        let original = items(&["alpha", "beta", "gamma"]);
        let ranked = promote_accepted(original.clone(), "jsh-test-unused-command");
        assert_eq!(
            ranked.iter().map(|c| c.text.as_str()).collect::<Vec<_>>(),
            vec!["alpha", "beta", "gamma"]
        );

        // A single candidate is never reordered, and neither is a command
        // name position (the caller passes an empty command for that).
        assert_eq!(promote_accepted(items(&["only"]), "git").len(), 1);
        assert_eq!(
            promote_accepted(items(&["a", "b"]), "")
                .iter()
                .map(|c| c.text.as_str())
                .collect::<Vec<_>>(),
            vec!["a", "b"]
        );
    }

    #[test]
    fn the_command_being_completed_is_identified_for_recording() {
        let mut state = ShellState::new(false);
        state
            .aliases
            .insert("gco".to_string(), "git checkout".to_string());

        // Argument positions name their command, alias expanded.
        let buffer = "git checkout ma";
        assert_eq!(
            command_at(buffer, buffer.len(), &state).as_deref(),
            Some("git")
        );
        let buffer = "gco ma";
        assert_eq!(
            command_at(buffer, buffer.len(), &state).as_deref(),
            Some("git")
        );
        let buffer = "ls | grep fo";
        assert_eq!(
            command_at(buffer, buffer.len(), &state).as_deref(),
            Some("grep")
        );
        let buffer = "sudo systemctl restart ng";
        assert_eq!(
            command_at(buffer, buffer.len(), &state).as_deref(),
            Some("systemctl")
        );

        // A command position has no command to attribute the choice to.
        assert_eq!(command_at("gi", 2, &state), None);
        assert_eq!(command_at("ls | gr", 7, &state), None);
    }

    #[test]
    fn test_operators_modes_and_man_sections_complete() {
        let mut state = ShellState::new(false);

        clear_cache();
        let buffer = "[[ -z";
        let (_, completions) = complete(buffer, buffer.len(), &mut state);
        let empty = completions.iter().find(|item| item.text == "-z").unwrap();
        assert_eq!(empty.description.as_deref(), Some("the string is empty"));

        clear_cache();
        let buffer = "if [ -d";
        let (_, completions) = complete(buffer, buffer.len(), &mut state);
        assert!(completions.iter().any(|item| item.text == "-d"));

        clear_cache();
        let buffer = "test -n";
        let (_, completions) = complete(buffer, buffer.len(), &mut state);
        assert!(completions.iter().any(|item| item.text == "-ne"));

        clear_cache();
        let buffer = "chmod 7";
        let (_, completions) = complete(buffer, buffer.len(), &mut state);
        let mode = completions.iter().find(|item| item.text == "755").unwrap();
        assert!(mode.description.as_deref().unwrap().contains("rwxr-xr-x"));

        clear_cache();
        let buffer = "man 3";
        let (_, completions) = complete(buffer, buffer.len(), &mut state);
        assert_eq!(
            completions
                .iter()
                .find(|item| item.text == "3")
                .and_then(|item| item.description.as_deref()),
            Some("library functions")
        );

        // `man ls` is still a command name, not a section.
        clear_cache();
        let buffer = "man ec";
        let (_, completions) = complete(buffer, buffer.len(), &mut state);
        assert!(completions.iter().any(|item| item.text == "echo"));
    }

    #[test]
    fn z_completes_frecent_directories_by_substring() {
        let entries = vec![
            ("/home/user/projects/jsh".to_string(), 9.0),
            ("/var/log".to_string(), 3.0),
        ];
        let completions = z_entry_completions(&entries, "jsh");
        assert_eq!(completions.len(), 1);
        assert_eq!(completions[0].text, "/home/user/projects/jsh");

        assert_eq!(z_entry_completions(&entries, "").len(), 2);
        assert!(z_entry_completions(&entries, "nothing").is_empty());
    }
}
