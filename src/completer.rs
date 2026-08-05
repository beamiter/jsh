/// Tab completion engine: context-aware completion for commands, paths, variables,
/// with configurable completion specs (Phase 7).
use crate::environment::ShellState;
use std::collections::HashMap;
use std::fs;
use std::path::Path;

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
}

pub fn complete(buffer: &str, cursor: usize, state: &mut ShellState) -> (usize, Vec<Completion>) {
    let buf = &buffer[..cursor];
    let (word, word_start) = extract_word_at(buf);
    let is_cmd_pos = is_command_position(buf, word_start);

    let cmd = resolve_transparent_alias(first_command(buf), &state.aliases);

    // `sudo -u <TAB>` sits at what the wrapper-skipping logic counts as a
    // command position; the value the option expects is a user or group.
    let wrapper_value = wrapper_value_kind(&buf[..word_start]);

    // Create cache key based on context
    let cache_key = if let Some(kind) = wrapper_value {
        format!("wrapval:{kind:?}:{word}")
    } else if is_cmd_pos {
        format!("cmd:{}", word)
    } else if word.starts_with('$') {
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

    let completions = if let Some(variable) = word.strip_prefix("${") {
        complete_variable_braced(variable, state)
    } else if let Some(variable) = word.strip_prefix('$') {
        complete_variable(variable, state)
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
    } else if let Some(subs) = subcommand_completions(&cmd, &word, buf, word_start, state) {
        subs
    } else if let Some(spec_completions) = complete_from_spec(&cmd, &word, buf, state) {
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
        let results = complete_path(&word, state);
        if results.is_empty() {
            complete_history_arguments(&cmd, &word, state)
        } else {
            results
        }
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
    buf: &str,
    word_start: usize,
    state: &mut ShellState,
) -> Option<Vec<Completion>> {
    let segment_start = active_command_segment_start(&buf[..word_start]);
    let before = buf[segment_start..word_start].trim();
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

    if matches!(cmd, "which" | "type" | "whereis" | "man") && !prefix.starts_with('-') {
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

    // First-level subcommands with descriptions
    if word_count == 1 {
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
            "checkout" | "switch" | "merge" | "rebase" | "branch" | "diff" | "log" => {
                // All refs, prefix matches first, fuzzy when nothing starts
                // with the typed text (`git checkout rel21` → release-2.1).
                return Some(rank_prefix_then_fuzzy(complete_git_refs(""), prefix));
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
            "mkdir" => &[("-p", "parents"), ("-m", "mode"), ("-v", "verbose")],
            "chmod" => &[
                ("-r", "recursive"),
                ("-v", "verbose"),
                ("-c", "changes only"),
                ("-R", "recursive"),
            ],
            _ => return None,
        };

        let completions = options
            .iter()
            .filter(|(opt, _)| opt.starts_with(prefix))
            .map(|(opt, desc)| Completion {
                text: opt.to_string(),
                display: opt.to_string(),
                description: Some(desc.to_string()),
                kind: CompletionKind::Flag,
                is_dir: false,
            })
            .collect::<Vec<_>>();

        if !completions.is_empty() {
            return Some(completions);
        }
    }

    None
}

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
    if let Ok(content) = crate::io_guard::read_regular_text(
        &home.join(".ssh/config"),
        MAX_COMPLETION_PROJECT_FILE_BYTES,
    ) {
        for host in parse_ssh_config_hosts(&content) {
            candidates.push((host, "ssh config"));
        }
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
    let mut hosts = Vec::new();
    for line in content.lines() {
        let line = line.trim();
        let Some(rest) = line
            .split_once(|ch: char| ch.is_whitespace() || ch == '=')
            .filter(|(keyword, _)| keyword.eq_ignore_ascii_case("host"))
            .map(|(_, rest)| rest)
        else {
            continue;
        };
        for pattern in rest.split_whitespace() {
            if pattern.contains('*') || pattern.contains('?') || pattern.starts_with('!') {
                continue;
            }
            hosts.push(pattern.to_string());
        }
    }
    hosts
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
    if let Some(output) = crate::prompt::bounded_git_stdout(
        Path::new("."),
        &[
            "for-each-ref",
            "--format=%(refname)",
            "refs/heads",
            "refs/remotes",
            "refs/tags",
        ],
        MAX_GIT_COMPLETION_BYTES,
    ) {
        return parse_git_refs(&String::from_utf8_lossy(&output), prefix);
    }
    Vec::new()
}

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
    if let Some(output) = crate::prompt::bounded_git_stdout(
        Path::new("."),
        &["status", "--porcelain=v1", "-z"],
        MAX_GIT_COMPLETION_BYTES,
    ) {
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
    if let Some(output) =
        crate::prompt::bounded_git_stdout(Path::new("."), &["remote"], MAX_GIT_COMPLETION_BYTES)
    {
        let stdout = String::from_utf8_lossy(&output);
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
    buf: &str,
    state: &ShellState,
) -> Option<Vec<Completion>> {
    use crate::completion_spec::SpecCompletionKind;

    let segment = active_command_segment(buf);
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
        match arg.template {
            ArgTemplate::FilePath => completions.extend(complete_path(prefix, state)),
            ArgTemplate::FolderPath => completions.extend(
                complete_path(prefix, state)
                    .into_iter()
                    .filter(|completion| completion.is_dir),
            ),
            _ => {}
        }
    }
    completions
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

fn complete_command(prefix: &str, state: &mut ShellState) -> Vec<Completion> {
    let mut completions = Vec::new();

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

    // Add path completions if prefix contains /
    if prefix.contains('/') {
        completions.extend(complete_path(prefix, state));
    }

    // Remove duplicates
    completions.dedup_by(|a, b| a.text == b.text);

    // Apply fuzzy filtering and sorting
    let filtered = filter_completions(completions, prefix);

    // Limit to top 50 completions to avoid overwhelming the user
    filtered.into_iter().take(50).collect()
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
    let expanded = if let Some(home_relative) = lookup_prefix.strip_prefix('~') {
        let home = state.home_dir.to_string_lossy();
        if lookup_prefix == "~" {
            format!("{}/", home)
        } else {
            format!("{home}{home_relative}")
        }
    } else {
        lookup_prefix.clone()
    };

    let (dir, file_prefix) = if expanded.ends_with('/') {
        (expanded.as_str(), "")
    } else {
        match expanded.rfind('/') {
            Some(pos) => (&expanded[..=pos], &expanded[pos + 1..]),
            None => (".", expanded.as_str()),
        }
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
            let full = if dir == "." {
                if is_dir {
                    format!("{}/", name)
                } else {
                    name.clone()
                }
            } else if lookup_prefix.starts_with('~') {
                let suffix = if expanded.ends_with('/') {
                    format!("{}{}", &lookup_prefix, name)
                } else {
                    match lookup_prefix.rfind('/') {
                        Some(pos) => format!("{}/{}", &lookup_prefix[..pos], name),
                        None => format!("~/{}", name),
                    }
                };
                if is_dir {
                    format!("{}/", suffix)
                } else {
                    suffix
                }
            } else {
                let path = if expanded.ends_with('/') {
                    format!("{}{}", lookup_prefix, name)
                } else {
                    match lookup_prefix.rfind('/') {
                        Some(pos) => format!("{}/{}", &lookup_prefix[..pos], name),
                        None => name.clone(),
                    }
                };
                if is_dir {
                    format!("{}/", path)
                } else {
                    path
                }
            };

            let description = path_metadata_desc(&entry);

            let completion = Completion {
                text: escape_shell_word(&full),
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
    if pattern.is_empty() {
        return 1000; // Empty pattern matches everything with high score
    }

    let text_lower = text.to_lowercase();
    let pattern_lower = pattern.to_lowercase();

    // Exact prefix match: highest score
    if text_lower.starts_with(&pattern_lower) {
        return 1000 - (text_lower.len() as i32 - pattern_lower.len() as i32).abs();
    }

    // Check if all characters of pattern exist in text in order
    let mut pattern_chars = pattern_lower.chars().peekable();
    let mut text_chars = text_lower.chars();
    let mut last_match_pos = 0;
    let mut match_count = 0;
    let mut gap_penalty = 0;

    for (pos, text_char) in text_chars.by_ref().enumerate() {
        if let Some(&pattern_char) = pattern_chars.peek() {
            if text_char == pattern_char {
                pattern_chars.next();
                match_count += 1;

                // Penalty for gaps between matches
                gap_penalty += pos.saturating_sub(last_match_pos).saturating_sub(1) as i32;
                last_match_pos = pos;

                // Bonus for consecutive matches
                if pos > 0
                    && text_lower
                        .chars()
                        .nth(pos - 1)
                        .map(|c| c == pattern_lower.chars().next().unwrap())
                        .unwrap_or(false)
                {
                    gap_penalty = gap_penalty.saturating_sub(5);
                }
            }
        }
    }

    if match_count == pattern_lower.len() {
        // All characters matched, score based on gaps and position
        500 + (match_count as i32 * 10) - gap_penalty
    } else {
        0 // No match
    }
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
    let mut scored: Vec<(Completion, i32)> = completions
        .into_iter()
        .map(|c| {
            let score = fuzzy_match_score(&c.text, pattern);
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
}

const MAX_HISTORY_ARG_RESULTS: usize = 10;

/// Arguments this command has been given before, for the fallback when no
/// path matches. `git checkout release-2<TAB>` finds the branch spelling from
/// history even though nothing in the working tree matches.
fn complete_history_arguments(cmd: &str, prefix: &str, state: &ShellState) -> Vec<Completion> {
    if cmd.is_empty() {
        return Vec::new();
    }
    let entries = crate::history::History::load_default_entries(10_000);
    let cwd = std::env::current_dir()
        .ok()
        .map(|p| p.to_string_lossy().to_string());
    history_argument_completions(&entries, cmd, &state.aliases, prefix, cwd.as_deref())
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
