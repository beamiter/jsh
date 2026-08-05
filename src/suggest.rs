use crate::history::History;
use crate::probe;
/// Auto-suggestion engine: fish-style ghost text from history + z-jump,
/// with context-aware git suggestions and sequential command recommendations.
use std::collections::HashMap;

/// Context passed to the suggestion engine (zero-allocation, borrows from ShellState).
#[derive(Default)]
pub struct SuggestionContext<'a> {
    pub git_branch: Option<&'a str>,
    pub git_remote: Option<&'a str>,
    /// Local branches, most recently committed first.
    pub git_branches: &'a [String],
    pub git_has_staged: bool,
    pub git_has_unstaged: bool,
    pub git_has_conflicts: bool,
    pub git_ahead: usize,
    pub git_behind: usize,
    pub last_command: Option<&'a str>,
    pub last_exit_code: i32,
}

/// Static command chain patterns: (prefix of last command) -> suggested next command.
/// Order matters: first match wins.
const COMMAND_CHAINS: &[(&str, &str)] = &[
    ("git commit", "git push"),
    ("git add", "git commit"),
    ("git stash pop", "git diff"),
    ("git stash", "git stash pop"),
    ("git pull", "git diff"),
    ("git clone", "cd "),
    ("cargo build", "cargo run"),
    ("cargo test", "cargo build"),
    ("cargo fmt", "cargo clippy"),
    ("docker build", "docker run"),
    ("mkdir", "cd "),
    ("npm install", "npm run"),
    ("make", "make install"),
];

/// Subcommand abbreviation suggestions: (command, [(abbreviation, full_subcommand), ...])
/// Suggests full subcommand names from common abbreviations.
const SUBCOMMAND_SUGGESTIONS: &[(&str, &[(&str, &str)])] = &[
    (
        "git",
        &[
            ("a", "add"),
            ("b", "branch"),
            ("bi", "bisect"),
            ("bl", "blame"),
            ("c", "commit"),
            ("ch", "checkout"),
            ("che", "cherry-pick"),
            ("cl", "clone"),
            ("d", "diff"),
            ("f", "fetch"),
            ("l", "log"),
            ("m", "merge"),
            ("mv", "mv"),
            ("p", "push"),
            ("pl", "pull"),
            ("r", "reflog"),
            ("re", "rebase"),
            ("rem", "remote"),
            ("res", "reset"),
            ("rev", "revert"),
            ("rm", "rm"),
            ("s", "status"),
            ("sh", "show"),
            ("st", "stash"),
            ("sw", "switch"),
            ("t", "tag"),
        ],
    ),
    (
        "cargo",
        &[
            ("b", "build"),
            ("c", "check"),
            ("cl", "clean"),
            ("d", "doc"),
            ("f", "fmt"),
            ("i", "init"),
            ("n", "new"),
            ("r", "run"),
            ("t", "test"),
            ("u", "update"),
        ],
    ),
    (
        "docker",
        &[
            ("b", "build"),
            ("c", "container"),
            ("e", "exec"),
            ("i", "images"),
            ("l", "logs"),
            ("p", "ps"),
            ("pu", "pull"),
            ("r", "run"),
            ("rm", "rm"),
            ("s", "start"),
            ("st", "stop"),
            ("v", "volume"),
        ],
    ),
    (
        "kubectl",
        &[
            ("a", "apply"),
            ("c", "create"),
            ("d", "delete"),
            ("des", "describe"),
            ("e", "exec"),
            ("g", "get"),
            ("l", "logs"),
            ("r", "run"),
        ],
    ),
    (
        "npm",
        &[
            ("i", "install"),
            ("r", "run"),
            ("s", "start"),
            ("t", "test"),
            ("u", "update"),
        ],
    ),
    (
        "systemctl",
        &[
            ("e", "enable"),
            ("d", "disable"),
            ("r", "restart"),
            ("s", "status"),
            ("sta", "start"),
            ("sto", "stop"),
        ],
    ),
];

/// Given the current buffer, find a suggestion from history, git context, or z-jump.
/// Returns the suffix to display as ghost text (the part after the buffer).
pub fn suggest(buffer: &str, history: &History, ctx: &SuggestionContext) -> Option<String> {
    // 0. Empty buffer: proactive sequential command suggestion
    if buffer.is_empty() {
        return suggest_next_command(ctx, history);
    }

    // Everything below matches against the command being typed *now*: after
    // `cargo build && git p`, the active segment is `git p`, and a matched
    // suffix appends at the cursor exactly as it would for a lone `git p`.
    let segment = crate::completer::active_command_segment(buffer);

    // Repository context must beat history here. A command copied from another
    // repository may contain `main` while this repository is on `master` (or the
    // other way around).
    if segment.starts_with("git ") {
        if let Some(s) = suggest_git_command(segment, ctx) {
            return Some(s);
        }
    }

    // 1. Exact prefix match from history: the whole buffer first, then the
    // active segment when the buffer already holds earlier commands.
    let cwd = std::env::current_dir().ok();
    let cwd = cwd
        .as_deref()
        .and_then(std::path::Path::to_str)
        .unwrap_or("");
    if let Some(entry) = history.search_prefix_in_cwd(buffer, cwd) {
        return Some(entry[buffer.len()..].to_string());
    }
    if segment != buffer && !segment.is_empty() {
        if let Some(entry) = history.search_prefix_in_cwd(segment, cwd) {
            return Some(entry[segment.len()..].to_string());
        }
    }

    // 2. Subcommand abbreviation expansion (git l → git log, cargo b → cargo build)
    if let Some(s) = suggest_subcommand(segment) {
        return Some(s);
    }

    // 3. For "cd " and "z " commands, suggest from the z-jump database
    if let Some(current_arg) = segment
        .strip_prefix("cd ")
        .or_else(|| segment.strip_prefix("z "))
    {
        let query = current_arg.trim();
        if !query.is_empty() {
            if let Ok(db) = crate::zjump::get_z_db().lock() {
                if let Some(path) = db.query(&[query]) {
                    // If user's arg is a prefix of the z-jump path, complete it
                    if path.starts_with(current_arg) && path.len() > current_arg.len() {
                        return Some(path[current_arg.len()..].to_string());
                    }
                    // If the query is a suffix/substring of the path but not a prefix,
                    // show the full path as a hint (user typed a relative/partial path)
                    if path != current_arg {
                        return Some(format!(" # -> {}", path));
                    }
                }
            }
        }
    }

    // 4. Filesystem probe: context-aware completion based on command + filesystem state
    if let Some(suggestion) = probe_filesystem_suggestion(segment) {
        return Some(suggestion);
    }

    None
}

/// Probe the filesystem for context-aware completion based on command type.
/// This is the integration layer between the buffer parsing and the probe module.
fn probe_filesystem_suggestion(buffer: &str) -> Option<String> {
    // Parse buffer to extract command and current partial argument
    let trimmed = buffer.trim_start();
    let parts: Vec<&str> = trimmed.splitn(2, char::is_whitespace).collect();
    if parts.len() < 2 {
        return None; // No argument started yet
    }

    let cmd = parts[0];
    let args_part = parts[1];

    // Get the last argument being typed (handle pipes, semicolons, etc.)
    let last_arg = args_part
        .rsplit([' ', '\t', '|', ';', '&'])
        .next()
        .unwrap_or("")
        .trim();

    if last_arg.is_empty() || last_arg.starts_with('-') {
        return None; // Don't probe for empty args or flags
    }

    // Get current working directory
    let cwd = std::env::current_dir().ok()?;

    // Call the probe module to get the best filesystem completion
    let full_completion = probe::probe_filesystem(cmd, last_arg, &cwd)?;

    // Return only the suffix (the part after what the user has typed)
    if full_completion.len() > last_arg.len() && full_completion.starts_with(last_arg) {
        Some(full_completion[last_arg.len()..].to_string())
    } else {
        None
    }
}

/// Git-aware suggestions: auto-complete `git push/pull` with the tracking remote
/// and current branch.
fn suggest_git_command(buffer: &str, ctx: &SuggestionContext) -> Option<String> {
    let branch = ctx.git_branch?;
    let remote = ctx.git_remote.unwrap_or("origin");

    for cmd in &["git push", "git pull"] {
        // "git push" or "git pull" (no trailing space)
        if buffer == *cmd {
            return Some(format!(" {} {}", remote, branch));
        }
        // "git push " (with trailing space, no remote yet)
        let with_space = format!("{} ", cmd);
        if buffer == with_space {
            return Some(format!("{} {}", remote, branch));
        }
        // "git push origin" (no trailing space after origin)
        let with_remote = format!("{} {}", cmd, remote);
        if buffer == with_remote {
            return Some(format!(" {}", branch));
        }
        // "git push origin " (with trailing space, ready for branch)
        let remote_space = format!("{} {} ", cmd, remote);
        if buffer == remote_space {
            return Some(branch.to_string());
        }
        // "git push origin ma" -> suggest "ster" if branch is "master"
        if buffer.starts_with(&remote_space) {
            let partial = &buffer[remote_space.len()..];
            if !partial.is_empty() && branch.starts_with(partial) && branch.len() > partial.len() {
                return Some(branch[partial.len()..].to_string());
            }
        }
    }

    // Branch arguments. checkout/switch prefer the current branch; merge and
    // rebase never suggest it — merging a branch into itself is not a thing.
    // Other branches come most recently committed first.
    let branch_commands = [
        ("git checkout ", true),
        ("git switch ", true),
        ("git merge ", false),
        ("git rebase ", false),
    ];
    for (command, include_current) in branch_commands {
        let Some(partial) = buffer.strip_prefix(command) else {
            continue;
        };
        if partial.is_empty() || partial.starts_with('-') {
            continue;
        }
        if include_current {
            if let Some(suffix) = ghost_suffix(branch, partial) {
                return Some(suffix);
            }
        }
        for candidate in ctx.git_branches {
            if !include_current && candidate == branch {
                continue;
            }
            if let Some(suffix) = ghost_suffix(candidate, partial) {
                return Some(suffix);
            }
        }
    }

    None
}

/// The rest of `full` when `partial` is a proper prefix of it.
fn ghost_suffix(full: &str, partial: &str) -> Option<String> {
    (full.starts_with(partial) && full.len() > partial.len())
        .then(|| full[partial.len()..].to_string())
}

/// Suggest full subcommand from common abbreviations (git l → git log, cargo b → cargo build).
/// Checks if the buffer matches "<command> <abbreviation>" pattern and suggests the full subcommand.
fn suggest_subcommand(buffer: &str) -> Option<String> {
    // Parse buffer to extract command and partial subcommand
    let parts: Vec<&str> = buffer.splitn(2, char::is_whitespace).collect();
    if parts.len() != 2 {
        return None; // Need exactly "command subcommand_prefix"
    }

    let cmd = parts[0];
    let partial = parts[1];

    // Don't suggest if there's already a space after the subcommand (user is typing arguments)
    if partial.contains(' ') {
        return None;
    }

    // Don't suggest for flags
    if partial.starts_with('-') {
        return None;
    }

    // Find the command in our subcommand suggestions
    for (command, subcommands) in SUBCOMMAND_SUGGESTIONS {
        if *command != cmd {
            continue;
        }

        // Look for exact abbreviation match
        for (abbrev, full) in *subcommands {
            if *abbrev == partial {
                // Exact match: suggest the rest of the full subcommand
                return Some(full[abbrev.len()..].to_string());
            }
            // Prefix match: if full subcommand starts with partial, suggest the rest
            if full.starts_with(partial) && full.len() > partial.len() {
                return Some(full[partial.len()..].to_string());
            }
        }
    }

    None
}

/// Proactive suggestion when the buffer is empty: recommend the next command
/// based on static chain rules and learned history patterns.
fn suggest_next_command(ctx: &SuggestionContext, history: &History) -> Option<String> {
    let last_cmd = ctx.last_command?;

    // Only suggest after successful commands
    if ctx.last_exit_code != 0 {
        return None;
    }

    // Prefer live repository state over a generic static command chain.
    if last_cmd.starts_with("git status") {
        if ctx.git_has_conflicts {
            return Some("git diff --name-only --diff-filter=U".to_string());
        }
        if ctx.git_has_unstaged {
            return Some("git add -A".to_string());
        }
        if ctx.git_has_staged {
            return Some("git commit -m ".to_string());
        }
        if ctx.git_behind > 0 {
            return Some("git pull --rebase".to_string());
        }
        if ctx.git_ahead > 0 {
            return git_push_command(ctx);
        }
    }

    if last_cmd.starts_with("git add") {
        if ctx.git_has_staged && !ctx.git_has_conflicts {
            // Return the full commit prompt at once; the cursor is ready for the message.
            return Some("git commit -m ".to_string());
        }
        return Some("git status".to_string());
    }

    // Arriving in a directory: suggest what is usually run there.
    if matches!(command_base(last_cmd), "cd" | "z") {
        let cwd = std::env::current_dir().ok();
        if let Some(cwd) = cwd.as_deref().and_then(std::path::Path::to_str) {
            if let Some(command) = frequent_command_in_cwd(history, cwd) {
                return Some(command);
            }
        }
    }

    // 1. Check static chain patterns first
    for (prefix, suggestion) in COMMAND_CHAINS {
        if last_cmd.starts_with(prefix) {
            // Enrich git push with branch info
            if *suggestion == "git push" {
                if let Some(command) = git_push_command(ctx) {
                    return Some(command);
                }
            }
            return Some(suggestion.to_string());
        }
    }

    // 2. Fall back to history-based chain learning
    suggest_from_history_chains(last_cmd, history)
}

/// The command most often typed in this directory, when that habit is strong
/// enough to be worth ghosting (at least three occurrences). Ties go to the
/// most recent. Navigation commands never suggest more navigation.
fn frequent_command_in_cwd(history: &History, cwd: &str) -> Option<String> {
    let commands = history.commands_in_cwd(cwd);
    let mut counts: HashMap<&str, (u32, usize)> = HashMap::new();
    for (index, command) in commands.iter().enumerate() {
        if matches!(command_base(command), "cd" | "z" | "exit") {
            continue;
        }
        let entry = counts.entry(command).or_insert((0, 0));
        entry.0 += 1;
        entry.1 = index;
    }
    counts
        .into_iter()
        .filter(|(_, (count, _))| *count >= 3)
        .max_by_key(|(_, (count, last_seen))| (*count, *last_seen))
        .map(|(command, _)| command.to_string())
}

fn git_push_command(ctx: &SuggestionContext) -> Option<String> {
    let branch = ctx.git_branch?;
    let remote = ctx.git_remote.unwrap_or("origin");
    Some(format!("git push {} {}", remote, branch))
}

/// Learn command chains from history: find the most common successor command.
fn suggest_from_history_chains(last_cmd: &str, history: &History) -> Option<String> {
    let entries = history.entries();
    if entries.len() < 2 {
        return None;
    }

    let last_base = command_base(last_cmd);

    // Count successor commands
    let mut successors: HashMap<&str, u32> = HashMap::new();
    for window in entries.windows(2) {
        if command_base(window[0]) == last_base {
            *successors.entry(window[1]).or_insert(0) += 1;
        }
    }

    // Require at least 3 occurrences to avoid noise
    successors
        .into_iter()
        .filter(|(_, count)| *count >= 3)
        .max_by_key(|(_, count)| *count)
        .map(|(cmd, _)| cmd.to_string())
}

/// Extract the "base" of a command for chain matching.
/// For compound commands like "git commit", uses the first two words.
/// For simple commands like "ls", uses just the first word.
fn command_base(cmd: &str) -> &str {
    let trimmed = cmd.trim();
    let mut words = trimmed.split_whitespace();
    let first = match words.next() {
        Some(w) => w,
        None => return trimmed,
    };

    // For known multi-word command families, include the subcommand
    match first {
        "git" | "cargo" | "docker" | "kubectl" | "npm" | "pip" | "pip3" | "go" | "make" => {
            if let Some(second) = words.next() {
                // Return slice covering "first second"
                let start = first.as_ptr() as usize - trimmed.as_ptr() as usize;
                let end = second.as_ptr() as usize - trimmed.as_ptr() as usize + second.len();
                &trimmed[start..end]
            } else {
                first
            }
        }
        _ => first,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_suggest_subcommand_git_exact_match() {
        // Exact abbreviation match: "git l" → "og" (completing to "log")
        assert_eq!(suggest_subcommand("git l"), Some("og".to_string()));
        assert_eq!(suggest_subcommand("git r"), Some("eflog".to_string()));
        assert_eq!(suggest_subcommand("git c"), Some("ommit".to_string()));
        assert_eq!(suggest_subcommand("git s"), Some("tatus".to_string()));
        assert_eq!(suggest_subcommand("git p"), Some("ush".to_string()));
    }

    #[test]
    fn test_suggest_subcommand_git_prefix_match() {
        // Prefix match: "git ch" → "eckout" (completing to "checkout")
        assert_eq!(suggest_subcommand("git ch"), Some("eckout".to_string()));
        assert_eq!(suggest_subcommand("git re"), Some("flog".to_string())); // reflog
        assert_eq!(suggest_subcommand("git st"), Some("atus".to_string())); // status or stash
    }

    #[test]
    fn test_suggest_subcommand_cargo() {
        assert_eq!(suggest_subcommand("cargo b"), Some("uild".to_string()));
        assert_eq!(suggest_subcommand("cargo r"), Some("un".to_string()));
        assert_eq!(suggest_subcommand("cargo t"), Some("est".to_string()));
        assert_eq!(suggest_subcommand("cargo c"), Some("heck".to_string()));
    }

    #[test]
    fn test_suggest_subcommand_docker() {
        assert_eq!(suggest_subcommand("docker b"), Some("uild".to_string()));
        assert_eq!(suggest_subcommand("docker r"), Some("un".to_string()));
        assert_eq!(suggest_subcommand("docker e"), Some("xec".to_string()));
        assert_eq!(suggest_subcommand("docker p"), Some("s".to_string()));
    }

    #[test]
    fn test_suggest_subcommand_npm() {
        assert_eq!(suggest_subcommand("npm i"), Some("nstall".to_string()));
        assert_eq!(suggest_subcommand("npm r"), Some("un".to_string()));
        assert_eq!(suggest_subcommand("npm t"), Some("est".to_string()));
    }

    #[test]
    fn test_suggest_subcommand_no_match() {
        // Unknown command
        assert_eq!(suggest_subcommand("unknown l"), None);

        // Unknown abbreviation
        assert_eq!(suggest_subcommand("git xyz"), None);

        // No space (just command, no subcommand yet)
        assert_eq!(suggest_subcommand("git"), None);

        // Already has arguments (space after subcommand)
        assert_eq!(suggest_subcommand("git log --oneline"), None);
    }

    #[test]
    fn test_suggest_subcommand_flags() {
        // Should not suggest for flags
        assert_eq!(suggest_subcommand("git --version"), None);
        assert_eq!(suggest_subcommand("cargo -V"), None);
    }

    #[test]
    fn test_suggest_subcommand_full_subcommand() {
        // If user has already typed the full subcommand, no suggestion
        assert_eq!(suggest_subcommand("git log"), None);
        assert_eq!(suggest_subcommand("cargo build"), None);
    }

    #[test]
    fn git_push_and_pull_use_probed_branch_and_remote_in_one_suggestion() {
        let ctx = SuggestionContext {
            git_branch: Some("master"),
            git_remote: Some("upstream"),
            ..SuggestionContext::default()
        };

        assert_eq!(
            suggest_git_command("git push", &ctx),
            Some(" upstream master".to_string())
        );
        assert_eq!(
            suggest_git_command("git pull ", &ctx),
            Some("upstream master".to_string())
        );
    }

    #[test]
    fn next_command_after_commit_is_not_split_into_multiple_suggestions() {
        let history = History::new(0);
        let ctx = SuggestionContext {
            git_branch: Some("main"),
            last_command: Some("git commit -m 'done'"),
            ..SuggestionContext::default()
        };

        assert_eq!(
            suggest_next_command(&ctx, &history),
            Some("git push origin main".to_string())
        );
    }

    #[test]
    fn suggestions_follow_the_active_command_segment() {
        let history = History::new(0);
        let ctx = SuggestionContext::default();

        // Abbreviation expansion works after connectors and pipes.
        assert_eq!(
            suggest("cargo build && git p", &history, &ctx),
            Some("ush".to_string())
        );
        assert_eq!(
            suggest("ls | git ch", &history, &ctx),
            Some("eckout".to_string())
        );

        // Git context suggestions too: the segment is `git push`.
        let ctx = SuggestionContext {
            git_branch: Some("main"),
            git_remote: Some("origin"),
            ..SuggestionContext::default()
        };
        assert_eq!(
            suggest("echo hi && git push", &history, &ctx),
            Some(" origin main".to_string())
        );
    }

    #[test]
    fn history_matches_the_active_segment_when_the_buffer_holds_earlier_commands() {
        let mut history = History::new(100);
        history.add("git push origin release-2.1");
        let ctx = SuggestionContext::default();

        assert_eq!(
            suggest("make && git pu", &history, &ctx),
            Some("sh origin release-2.1".to_string())
        );
    }

    #[test]
    fn entering_a_directory_suggests_its_usual_command() {
        let mut history = History::new(100);
        history.add_with_cwd("cargo test", Some("/proj"));
        history.add_with_cwd("ls", Some("/proj"));
        history.add_with_cwd("cargo test", Some("/proj"));
        history.add_with_cwd("vim src/main.rs", Some("/proj"));
        history.add_with_cwd("cargo test", Some("/proj"));
        history.add_with_cwd("cd /proj", Some("/elsewhere"));

        assert_eq!(
            frequent_command_in_cwd(&history, "/proj"),
            Some("cargo test".to_string())
        );
        // Two occurrences are not a habit yet.
        assert_eq!(frequent_command_in_cwd(&history, "/elsewhere"), None);
    }

    #[test]
    fn checkout_ghosts_any_cached_branch_with_the_current_one_preferred() {
        let branches = vec!["feature-x".to_string(), "main".to_string()];
        let ctx = SuggestionContext {
            git_branch: Some("main"),
            git_branches: &branches,
            ..SuggestionContext::default()
        };

        // Another branch entirely: completed from the cached list.
        assert_eq!(
            suggest_git_command("git checkout fea", &ctx),
            Some("ture-x".to_string())
        );
        // The current branch still wins when both match.
        let both = vec!["maintenance".to_string(), "main".to_string()];
        let ctx = SuggestionContext {
            git_branch: Some("main"),
            git_branches: &both,
            ..SuggestionContext::default()
        };
        assert_eq!(
            suggest_git_command("git switch mai", &ctx),
            Some("n".to_string())
        );
    }

    #[test]
    fn merge_and_rebase_never_ghost_the_current_branch() {
        let branches = vec!["main".to_string(), "maintenance".to_string()];
        let ctx = SuggestionContext {
            git_branch: Some("main"),
            git_branches: &branches,
            ..SuggestionContext::default()
        };

        assert_eq!(
            suggest_git_command("git merge mai", &ctx),
            Some("ntenance".to_string())
        );
        assert_eq!(
            suggest_git_command("git rebase mai", &ctx),
            Some("ntenance".to_string())
        );
    }

    #[test]
    fn git_status_and_add_follow_live_worktree_state() {
        let history = History::new(0);
        let unstaged = SuggestionContext {
            git_has_unstaged: true,
            last_command: Some("git status"),
            ..SuggestionContext::default()
        };
        assert_eq!(
            suggest_next_command(&unstaged, &history),
            Some("git add -A".to_string())
        );

        let staged = SuggestionContext {
            git_has_staged: true,
            last_command: Some("git add -A"),
            ..SuggestionContext::default()
        };
        assert_eq!(
            suggest_next_command(&staged, &history),
            Some("git commit -m ".to_string())
        );
    }
}
