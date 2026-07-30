/// Tab completion engine: context-aware completion for commands, paths, variables,
/// with configurable completion specs (Phase 7).
use crate::environment::ShellState;
use std::collections::HashMap;
use std::fs;

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

    let mut cmd = first_command(buf);
    // A one-word alias is a transparent command wrapper for completion (`g=git`).
    for _ in 0..8 {
        let Some(expansion) = state.aliases.get(&cmd) else {
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

    // Create cache key based on context
    let cache_key = if is_cmd_pos {
        format!("cmd:{}", word)
    } else if word.starts_with('$') {
        format!("var:{}", &word[1..])
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
            let completions = apply_completion_spec(&spec, &word, state);
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

    let completions = if word.starts_with('$') {
        complete_variable(&word[1..], state)
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
    } else if cmd == "cd" || cmd == "mkdir" || cmd == "rmdir" || cmd == "z" {
        complete_path(&word, state)
            .into_iter()
            .filter(|c| c.is_dir)
            .collect()
    } else {
        complete_path(&word, state)
    };

    // Store in cache
    COMPLETION_CACHE.with(|cache| {
        cache.borrow_mut().insert(cache_key, completions.clone());
    });

    (word_start, completions)
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

fn is_assignment_word(word: &str) -> bool {
    let Some((name, _)) = word.split_once('=') else {
        return false;
    };
    let mut chars = name.chars();
    matches!(chars.next(), Some(ch) if ch == '_' || ch.is_ascii_alphabetic())
        && chars.all(|ch| ch == '_' || ch.is_ascii_alphanumeric())
}

fn active_command_segment(buf: &str) -> &str {
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
    state: &ShellState,
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
                let results = complete_cargo_argument(value, kind)
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
            let results = complete_npm_scripts(prefix);
            if !results.is_empty() {
                return Some(results);
            }
        }
        if cmd == "make" && word_count == 1 {
            let results = complete_make_targets(prefix);
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
                let results = complete_cargo_argument(prefix, kind);
                if !results.is_empty() {
                    return Some(results);
                }
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
                    .filter(|n| n.starts_with(prefix))
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
                return Some(completions);
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
            .filter(|(name, _)| name.starts_with(prefix))
            .map(|(name, desc)| Completion {
                text: name.to_string(),
                display: name.to_string(),
                description: Some(desc.to_string()),
                kind: CompletionKind::Subcommand,
                is_dir: false,
            })
            .collect::<Vec<_>>();

        return Some(completions);
    }

    // Second-level: git context-aware completions
    // Flags come from the richer JSON command spec below. Dynamic Git argument
    // completion must not swallow inputs such as `git push -` or `git switch -`.
    if cmd == "git" && word_count >= 2 && !prefix.starts_with('-') {
        let subcmd = words.get(1).copied().unwrap_or("");
        match subcmd {
            "checkout" | "switch" | "merge" | "rebase" | "branch" | "diff" | "log" => {
                return Some(complete_git_refs(prefix));
            }
            "add" => {
                return Some(complete_git_dirty_files(prefix, "add"));
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
                return Some(complete_git_dirty_files(prefix, context));
            }
            "reset" => {
                let mut results = complete_git_refs(prefix);
                results.extend(complete_git_dirty_files(prefix, "reset"));
                return Some(results);
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
                    .filter(|(name, _)| name.starts_with(prefix))
                    .map(|(name, desc)| Completion {
                        text: name.to_string(),
                        display: name.to_string(),
                        description: Some(desc.to_string()),
                        kind: CompletionKind::Subcommand,
                        is_dir: false,
                    })
                    .collect();
                return Some(completions);
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
                    .filter(|(name, _)| name.starts_with(prefix))
                    .map(|(name, desc)| Completion {
                        text: name.to_string(),
                        display: name.to_string(),
                        description: Some(desc.to_string()),
                        kind: CompletionKind::Subcommand,
                        is_dir: false,
                    })
                    .collect();
                return Some(completions);
            }
            "remote" if word_count >= 3 => {
                return Some(complete_git_remotes(prefix));
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
                .filter(|s| s.starts_with(prefix))
                .map(|s| Completion {
                    text: s.to_string(),
                    display: s.to_string(),
                    description: None,
                    kind: CompletionKind::Subcommand,
                    is_dir: false,
                })
                .collect::<Vec<_>>();
            return Some(completions);
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
                    .filter(|n| n.starts_with(prefix))
                    .map(|n| Completion {
                        text: n.clone(),
                        display: n,
                        description: Some("bookmark".to_string()),
                        kind: CompletionKind::Other,
                        is_dir: false,
                    })
                    .collect::<Vec<_>>();
                return Some(completions);
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
    if let Ok(output) = std::process::Command::new("git")
        .args([
            "for-each-ref",
            "--format=%(refname)",
            "refs/heads",
            "refs/remotes",
            "refs/tags",
        ])
        .output()
    {
        if output.status.success() {
            return parse_git_refs(&String::from_utf8_lossy(&output.stdout), prefix);
        }
    }
    Vec::new()
}

fn parse_git_refs(output: &str, prefix: &str) -> Vec<Completion> {
    let mut completions = Vec::new();
    for reference in output.lines().map(str::trim) {
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
            if tag.starts_with(prefix) {
                if !completions.iter().any(|item| item.text == tag) {
                    completions.push(Completion {
                        text: tag.to_string(),
                        display: tag.to_string(),
                        description: Some("tag".to_string()),
                        kind: CompletionKind::Other,
                        is_dir: false,
                    });
                }
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
    if let Ok(output) = std::process::Command::new("git")
        .args(["status", "--porcelain=v1", "-z"])
        .output()
    {
        if output.status.success() {
            let decoded_prefix = unescape_shell_word(prefix);
            for (status, file) in parse_git_status_entries(&output.stdout) {
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
    }
    completions
}

/// Parse `git status --porcelain=v1 -z`. Rename/copy records contain a second
/// NUL-delimited source path; completion should insert the destination path.
fn parse_git_status_entries(output: &[u8]) -> Vec<([u8; 2], String)> {
    let fields: Vec<&[u8]> = output
        .split(|byte| *byte == 0)
        .filter(|field| !field.is_empty())
        .collect();
    let mut entries = Vec::new();
    let mut index = 0;
    while index < fields.len() {
        let field = fields[index];
        if field.len() < 4 || field[2] != b' ' {
            index += 1;
            continue;
        }
        let status = [field[0], field[1]];
        entries.push((status, String::from_utf8_lossy(&field[3..]).into_owned()));
        index += if status.iter().any(|code| matches!(code, b'R' | b'C')) {
            2
        } else {
            1
        };
    }
    entries
}

fn complete_git_stashes(prefix: &str) -> Vec<Completion> {
    let mut completions = Vec::new();
    if let Ok(output) = std::process::Command::new("git")
        .args(["stash", "list", "--format=%gd|%gs"])
        .output()
    {
        if output.status.success() {
            let stdout = String::from_utf8_lossy(&output.stdout);
            for line in stdout.lines() {
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
    }
    completions
}

fn complete_git_recent_commits(prefix: &str) -> Vec<Completion> {
    let mut completions = Vec::new();
    if let Ok(output) = std::process::Command::new("git")
        .args(["log", "--oneline", "-20", "--format=%h|%s"])
        .output()
    {
        if output.status.success() {
            let stdout = String::from_utf8_lossy(&output.stdout);
            for line in stdout.lines() {
                let parts: Vec<&str> = line.splitn(2, '|').collect();
                let (hash, msg) = if parts.len() == 2 {
                    (parts[0], parts[1])
                } else {
                    (line, "")
                };
                if hash.starts_with(prefix) || prefix.is_empty() {
                    let desc = if msg.len() > 40 {
                        format!("{}…", &msg[..39])
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
    }
    completions
}

fn complete_git_remotes(prefix: &str) -> Vec<Completion> {
    let mut completions = Vec::new();
    if let Ok(output) = std::process::Command::new("git").args(["remote"]).output() {
        if output.status.success() {
            let stdout = String::from_utf8_lossy(&output.stdout);
            for remote in stdout.lines() {
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
    let expanded = if lookup_prefix.starts_with('~') {
        let home = state.home_dir.to_string_lossy();
        if lookup_prefix == "~" {
            format!("{}/", home)
        } else {
            format!("{}{}", home, &lookup_prefix[1..])
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

    if let Ok(entries) = fs::read_dir(dir) {
        for entry in entries.flatten() {
            let name = entry.file_name().to_string_lossy().to_string();
            if !name.starts_with(file_prefix) {
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

            completions.push(Completion {
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
            });
        }
    }

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
    let before_pipe = buf.rsplitn(2, '|').nth(1).unwrap_or("").trim();
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
    let Ok(content) = fs::read_to_string(path) else {
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
    let Ok(content) = fs::read_to_string(manifest_path) else {
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
                        for path in paths.flatten() {
                            if let Ok(content) = fs::read_to_string(path) {
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
    let Ok(content) = fs::read_to_string(path) else {
        return Vec::new();
    };
    let mut targets = Vec::new();
    for line in content.lines() {
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
        assert!(!(before.ends_with('|') && !before.ends_with("||")));
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
}
