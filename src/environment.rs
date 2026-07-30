use std::collections::HashMap;
use std::env;
use std::path::PathBuf;
use std::sync::mpsc;

use serde::{Deserialize, Serialize};

use crate::job::JobTable;
use crate::parser::ast::CompoundCommand;

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub enum EditingMode {
    Emacs,
    Vi,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub enum ConfigSource {
    Bashrc, // 使用 .bashrc，直接用 bash 执行
    Jshrc,  // 使用 .jshrc，用 jsh 解析器执行
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum PromptStyle {
    Full,    // user@host ~/path (branch) took duration ❯
    Compact, // user ~/path (branch) ❯
    Minimal, // ~/path ❯
    Auto,    // Automatically choose based on terminal width
}

impl Default for PromptStyle {
    fn default() -> Self {
        PromptStyle::Auto
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ShellOpts {
    pub errexit: bool,               // set -e
    pub xtrace: bool,                // set -x
    pub pipefail: bool,              // set -o pipefail
    pub globstar: bool,              // set -o globstar
    pub dotglob: bool,               // shopt dotglob: match hidden files
    pub nullglob: bool,              // shopt nullglob: empty string for no matches
    pub failglob: bool,              // shopt failglob: error on no matches
    pub extglob: bool,               // shopt extglob: extended glob patterns
    pub nocaseglob: bool,            // shopt nocaseglob: case-insensitive matching
    pub noglob: bool,                // shopt noglob: disable pathname expansion
    pub lastpipe: bool,              // shopt lastpipe: last pipe component in current shell
    pub autocd: bool,                // shopt autocd: cd to bare directory names
    pub cdspell: bool,               // shopt cdspell: correct cd spelling errors
    pub checkwinsize: bool,          // shopt checkwinsize: update LINES/COLUMNS
    pub inherit_errexit: bool,       // shopt inherit_errexit: subshells inherit errexit
    pub config_source: ConfigSource, // which config file to use: .bashrc or .jshrc
}

impl Default for ShellOpts {
    fn default() -> Self {
        ShellOpts {
            errexit: false,
            xtrace: false,
            pipefail: false,
            globstar: true,
            dotglob: false,
            nullglob: false,
            failglob: false,
            extglob: false,
            nocaseglob: false,
            noglob: false,
            lastpipe: false,
            autocd: false,
            cdspell: false,
            checkwinsize: false,
            inherit_errexit: false,
            config_source: ConfigSource::Bashrc,
            tracked_opts: HashMap::new(),
        }
    }
}

/// True if `name` is a shell identifier (`[A-Za-z_][A-Za-z0-9_]*`) — the only
/// spelling bash's `export`, `unset` and `declare` accept as a variable name.
pub fn is_valid_identifier(name: &str) -> bool {
    let mut chars = name.chars();
    match chars.next() {
        Some(c) if c == '_' || c.is_ascii_alphabetic() => {}
        _ => return false,
    }
    chars.all(|c| c == '_' || c.is_ascii_alphanumeric())
}

/// True if `name` may be handed to the C environment.
///
/// `std::env::set_var` / `remove_var` PANIC — killing the whole shell with exit
/// 101 — on an empty name, a name containing `=`, or an interior NUL. A typo
/// such as `export =` must not be able to do that, so every call goes through
/// the two helpers below.
fn env_name_ok(name: &str) -> bool {
    !name.is_empty() && !name.contains('=') && !name.contains('\0')
}

fn set_env_checked(name: &str, value: &str) {
    if env_name_ok(name) && !value.contains('\0') {
        env::set_var(name, value);
    }
}

fn remove_env_checked(name: &str) {
    if env_name_ok(name) {
        env::remove_var(name);
    }
}

/// Hook lists for shell events.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct ShellHooks {
    pub precmd: Vec<String>,  // run before each prompt
    pub preexec: Vec<String>, // run before each command
    pub chpwd: Vec<String>,   // run after directory change
}

/// Completion specification for a command.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CompletionSpec {
    pub command: String,
    pub word_list: Option<Vec<String>>,
    pub function: Option<String>,
    pub directory: bool,
    pub file: bool,
    pub glob_pattern: Option<String>,
    pub filter_pattern: Option<String>,
    pub prefix: Option<String>,
    pub suffix: Option<String>,
}

pub struct ShellState {
    pub env_vars: HashMap<String, String>,
    pub local_vars_stack: Vec<HashMap<String, String>>,
    pub aliases: HashMap<String, String>,
    pub functions: HashMap<String, CompoundCommand>,
    pub last_exit_code: i32,
    pub last_bg_pid: Option<u32>,
    pub interactive: bool,
    pub home_dir: PathBuf,
    pub hostname: String,
    path_cache: Option<Vec<String>>,
    path_hash: u64,
    path_scan_rx: Option<mpsc::Receiver<Vec<String>>>,
    /// Invocation name exposed as shell parameter `$0`.
    ///
    /// Unlike ordinary positional parameters, this is preserved across shell
    /// function calls and `source` invocations.
    pub arg0: String,
    pub positional_params: Vec<String>,
    pub positional_stack: Vec<Vec<String>>,
    pub dir_stack: Vec<PathBuf>,
    pub shell_opts: ShellOpts,
    pub traps: HashMap<String, String>,
    pub pipestatus: Vec<i32>,
    /// Deferred word-expansion failure (for example an unmatched failglob).
    /// Expansion returns vectors for compatibility, so the executor consumes
    /// this immediately after `expand_words`.
    pub expansion_error: Option<String>,
    /// Stop the remainder of the currently parsed program after a fatal
    /// expansion error. Unlike `errexit`, failglob aborts the list even when
    /// `set -e` is disabled.
    pub abort_current_program: bool,
    /// Nesting depth for contexts where Bash ignores `errexit` (the tested
    /// operands of `&&`, `||`, `!`, and shell-condition command lists).
    pub errexit_suppression_depth: usize,
    // PIDs of process-substitution children awaiting non-blocking reaping.
    pub procsub_pids: Vec<i32>,
    pub jobs: JobTable,
    // Arrays (Phase 1)
    pub arrays: HashMap<String, Vec<String>>,
    pub assoc_arrays: HashMap<String, HashMap<String, String>>,
    // Hooks (Phase 4)
    pub hooks: ShellHooks,
    // Completion specs (Phase 7)
    pub completion_specs: HashMap<String, CompletionSpec>,
    // Notification threshold (Phase 8)
    pub notification_threshold: std::time::Duration,
    // Last command duration for rprompt
    pub last_command_duration: Option<std::time::Duration>,
    // Editing mode (vi or emacs)
    pub editing_mode: EditingMode,
    // Prompt style and terminal width
    pub prompt_style: PromptStyle,
    pub terminal_width: usize,
    // Loop control flow (break/continue)
    pub loop_break: bool,
    pub loop_continue: bool,
    /// Number of active loop constructs in the current execution context.
    pub loop_depth: usize,
    // Function return control flow
    pub return_requested: bool,
    pub return_value: i32,
    /// Number of active function/source contexts in which `return` is valid.
    pub return_depth: usize,
    /// Last executed command line (for sequential command suggestions)
    pub last_command: Option<String>,
    /// Cached git branch for current directory (avoids filesystem I/O per keystroke)
    pub cached_git_branch: Option<String>,
    /// Tracking remote for the current branch, discovered together with the branch.
    pub cached_git_remote: Option<String>,
    /// Git worktree state, refreshed once before each prompt.
    pub cached_git_has_staged: bool,
    pub cached_git_has_unstaged: bool,
    pub cached_git_has_conflicts: bool,
    pub cached_git_ahead: usize,
    pub cached_git_behind: usize,
    /// Extensible completion spec registry (Phase 3: spec system)
    pub spec_registry: crate::completion_spec::SpecRegistry,
    /// Workflow registry (Phase 4: parameterized command templates)
    pub workflow_registry: crate::workflows::WorkflowRegistry,
    /// Typed let-bindings (Phase 5a foundation; populated by `let` in 5b).
    pub let_vars: HashMap<String, crate::value::Value>,
    /// Phase 5b: closure literals (`{|x|...}`) seen as command arguments are
    /// stashed here and referenced via a sentinel string returned by expansion.
    /// Cleared at the start of each top-level command so it does not grow.
    pub inline_closures: Vec<std::sync::Arc<crate::value::ClosureData>>,
    /// Number of fork() calls executed for pipelines (test instrumentation).
    pub fork_count: std::sync::Arc<std::sync::atomic::AtomicU64>,
    /// Phase 14a: structured error from the most recent failing value-aware
    /// builtin or closure body. Populated alongside the conventional i32 exit
    /// code so `try { ... } catch { |e| ... }` can surface a Record `{msg, code}`
    /// to the handler. Cleared on success and at the entry of each `try` block.
    pub last_error: Option<crate::value::Value>,
    /// Phase 15c: signatures registered for user-defined functions via `def`.
    /// Looked up alongside the static `SIGNATURES` so `help my-fn` works and
    /// `my-fn` calls can validate args.
    pub user_signatures: HashMap<String, crate::signature::RuntimeSignature>,
    /// Phase 15c: typed user functions — name → closure body. The closure
    /// captures let_vars at def time, like inline `{||...}` closures.
    pub user_typed_fns: HashMap<String, std::sync::Arc<crate::value::ClosureData>>,
    /// Command text a builtin (the agent's insert-only review path) asks the
    /// editor to prefill into the next prompt. Review-only: taking this never
    /// executes anything.
    pub pending_editor_insert: Option<String>,
}

impl ShellState {
    pub fn new(interactive: bool) -> Self {
        let mut env_vars = HashMap::new();
        for (k, v) in env::vars() {
            env_vars.insert(k, v);
        }
        // Bash always starts with IFS at its default. Scripts routinely save and
        // restore it (`old=$IFS; ...; IFS=$old`), so leaving it unset would let
        // that idiom clobber IFS to the empty string and kill word splitting for
        // the rest of the session.
        env_vars.insert("IFS".to_string(), " \t\n".to_string());
        let mut aliases = HashMap::new();
        if interactive {
            aliases.insert("ls".to_string(), "ls --color=auto".to_string());
            aliases.insert("ll".to_string(), "ls -alF --color=auto".to_string());
            aliases.insert("la".to_string(), "ls -A --color=auto".to_string());
        }

        let home_dir = dirs::home_dir().unwrap_or_else(|| PathBuf::from("/"));
        let hostname = std::fs::read_to_string("/etc/hostname")
            .map(|s| s.trim().to_string())
            .unwrap_or_else(|_| String::from("localhost"));

        let path_hash = Self::hash_path(env_vars.get("PATH").map(|s| s.as_str()).unwrap_or(""));
        let arg0 = env::args().next().unwrap_or_else(|| "jsh".to_string());

        ShellState {
            env_vars,
            local_vars_stack: Vec::new(),
            aliases,
            functions: HashMap::new(),
            last_exit_code: 0,
            last_bg_pid: None,
            interactive,
            home_dir,
            hostname,
            path_cache: None,
            path_hash,
            path_scan_rx: None,
            arg0,
            positional_params: Vec::new(),
            positional_stack: Vec::new(),
            dir_stack: Vec::new(),
            shell_opts: ShellOpts::default(),
            traps: HashMap::new(),
            pipestatus: Vec::new(),
            expansion_error: None,
            abort_current_program: false,
            errexit_suppression_depth: 0,
            procsub_pids: Vec::new(),
            jobs: JobTable::new(),
            arrays: HashMap::new(),
            assoc_arrays: HashMap::new(),
            hooks: ShellHooks::default(),
            completion_specs: HashMap::new(),
            notification_threshold: std::time::Duration::from_secs(10),
            last_command_duration: None,
            editing_mode: EditingMode::Emacs,
            prompt_style: PromptStyle::Auto,
            terminal_width: Self::detect_terminal_width(),
            loop_break: false,
            loop_continue: false,
            loop_depth: 0,
            return_requested: false,
            return_value: 0,
            return_depth: 0,
            last_command: None,
            cached_git_branch: None,
            cached_git_remote: None,
            cached_git_has_staged: false,
            cached_git_has_unstaged: false,
            cached_git_has_conflicts: false,
            cached_git_ahead: 0,
            cached_git_behind: 0,
            spec_registry: crate::completion_spec::SpecRegistry::new(),
            workflow_registry: crate::workflows::WorkflowRegistry::new(),
            let_vars: HashMap::new(),
            inline_closures: Vec::new(),
            fork_count: std::sync::Arc::new(std::sync::atomic::AtomicU64::new(0)),
            last_error: None,
            user_signatures: HashMap::new(),
            user_typed_fns: HashMap::new(),
            pending_editor_insert: None,
        }
    }

    /// Set `$0` and the positional parameters for a new shell invocation.
    pub fn set_invocation(&mut self, arg0: impl Into<String>, args: &[String]) {
        self.arg0 = arg0.into();
        self.positional_params = args.to_vec();
    }

    pub fn take_expansion_error(&mut self) -> Option<String> {
        self.expansion_error.take()
    }

    /// Phase 14a: record a structured error. `msg` is the human-readable
    /// description; `code` is the conventional exit code the builtin returns
    /// alongside it. Stored as a Record so `catch { |e| $e.msg }` works.
    pub fn set_error(&mut self, msg: impl Into<String>, code: i32) {
        use crate::value::Value;
        let mut rec = indexmap::IndexMap::new();
        rec.insert("msg".to_string(), Value::String(msg.into()));
        rec.insert("code".to_string(), Value::Int(code as i64));
        self.last_error = Some(Value::Record(rec));
    }

    /// Phase 14a: pop the structured error (consumed by `try`).
    pub fn take_error(&mut self) -> Option<crate::value::Value> {
        self.last_error.take()
    }

    fn hash_path(path: &str) -> u64 {
        use std::collections::hash_map::DefaultHasher;
        use std::hash::{Hash, Hasher};

        let mut hasher = DefaultHasher::new();
        path.hash(&mut hasher);
        hasher.finish()
    }

    /// The `$-` string: one letter per currently enabled option, in the order
    /// bash prints them. Scripts test it to detect `set -e`/`set -x` before
    /// toggling an option and restoring it (nvm.sh does this on every call),
    /// so the letters jsh does not model simply never appear.
    pub fn option_flags(&self) -> String {
        let mut flags = String::new();
        if self.shell_opts.errexit {
            flags.push('e');
        }
        if self.shell_opts.noglob {
            flags.push('f');
        }
        // Command hashing and brace expansion are always on, as in bash.
        flags.push('h');
        if self.interactive {
            flags.push('i');
        }
        flags.push('B');
        if self.shell_opts.xtrace {
            flags.push('x');
        }
        flags
    }

    pub fn get_var(&self, name: &str) -> Option<&str> {
        match name {
            "?" => return None, // handled by expand
            _ => {}
        }
        // Check local_vars_stack from top to bottom
        for scope in self.local_vars_stack.iter().rev() {
            if let Some(val) = scope.get(name) {
                return Some(val.as_str());
            }
        }
        // Fall back to env_vars
        self.env_vars.get(name).map(|s| s.as_str())
    }

    fn detect_terminal_width() -> usize {
        // Try COLUMNS environment variable first
        if let Ok(cols_str) = env::var("COLUMNS") {
            if let Ok(cols) = cols_str.parse::<usize>() {
                if cols > 0 {
                    return cols;
                }
            }
        }

        // Try stty size
        if let Ok(output) = std::process::Command::new("stty").arg("size").output() {
            if let Ok(output_str) = String::from_utf8(output.stdout) {
                let parts: Vec<&str> = output_str.split_whitespace().collect();
                if parts.len() >= 2 {
                    if let Ok(cols) = parts[1].parse::<usize>() {
                        if cols > 0 {
                            return cols;
                        }
                    }
                }
            }
        }

        // Default fallback
        80
    }

    pub fn set_var(&mut self, name: &str, value: &str) {
        if self.env_vars.contains_key(name) {
            self.env_vars.insert(name.to_string(), value.to_string());
            env::set_var(name, value);
            if name == "PATH" {
                self.invalidate_path_cache();
            }
        } else if let Some(scope) = self.local_vars_stack.last_mut() {
            // In function scope: set in current local scope
            scope.insert(name.to_string(), value.to_string());
        } else {
            // At global scope: set in env_vars
            self.env_vars.insert(name.to_string(), value.to_string());
            env::set_var(name, value);
            if name == "PATH" {
                self.invalidate_path_cache();
            }
        }
    }

    pub fn export_var(&mut self, name: &str, value: &str) {
        self.env_vars.insert(name.to_string(), value.to_string());
        env::set_var(name, value);
        // Remove from all local scopes
        for scope in &mut self.local_vars_stack {
            scope.remove(name);
        }
        if name == "PATH" {
            self.invalidate_path_cache();
        }
    }

    pub fn unset_var(&mut self, name: &str) {
        self.env_vars.remove(name);
        // Remove from all local scopes
        for scope in &mut self.local_vars_stack {
            scope.remove(name);
        }
        self.arrays.remove(name);
        self.assoc_arrays.remove(name);
        env::remove_var(name);
        if name == "PATH" {
            self.invalidate_path_cache();
        }
    }

    fn invalidate_path_cache(&mut self) {
        self.path_cache = None;
        // Update the hash for the next check
        self.path_hash =
            Self::hash_path(self.env_vars.get("PATH").map(|s| s.as_str()).unwrap_or(""));
    }

    pub fn path_cache(&mut self) -> &Vec<String> {
        let current_path_hash =
            Self::hash_path(self.env_vars.get("PATH").map(|s| s.as_str()).unwrap_or(""));

        if self.path_hash != current_path_hash {
            self.path_cache = None;
            self.path_scan_rx = None;
            self.path_hash = current_path_hash;
        }

        // Check for completed background scan
        if let Some(ref rx) = self.path_scan_rx {
            if let Ok(result) = rx.try_recv() {
                self.path_cache = Some(result);
                self.path_scan_rx = None;
            }
        }

        if self.path_cache.is_none() && self.path_scan_rx.is_none() {
            // Start async scan
            let path_val = self.env_vars.get("PATH").cloned().unwrap_or_default();
            let (tx, rx) = mpsc::channel();
            self.path_scan_rx = Some(rx);
            std::thread::spawn(move || {
                let mut cache = Vec::new();
                for dir in path_val.split(':') {
                    if let Ok(entries) = std::fs::read_dir(dir) {
                        for entry in entries.flatten() {
                            if let Ok(ft) = entry.file_type() {
                                if ft.is_file() || ft.is_symlink() {
                                    if let Some(name) = entry.file_name().to_str() {
                                        cache.push(name.to_string());
                                    }
                                }
                            }
                        }
                    }
                }
                cache.sort_unstable();
                cache.dedup();
                let _ = tx.send(cache);
            });
            // Return empty vec on first call (scan is in progress)
            // For immediate use, do a synchronous scan as fallback
            if self.path_cache.is_none() {
                self.path_cache = Some(Vec::new());
                // Block briefly for the first scan to complete
                if let Some(ref rx) = self.path_scan_rx {
                    if let Ok(result) = rx.recv_timeout(std::time::Duration::from_millis(100)) {
                        self.path_cache = Some(result);
                        self.path_scan_rx = None;
                    }
                }
            }
        }
        self.path_cache.as_ref().unwrap()
    }

    pub fn command_in_path(&mut self, name: &str) -> bool {
        self.path_cache().binary_search(&name.to_string()).is_ok()
    }

    pub fn push_positional_params(&mut self, args: Vec<String>) {
        self.positional_stack
            .push(std::mem::replace(&mut self.positional_params, args));
    }

    pub fn pop_positional_params(&mut self) {
        if let Some(old) = self.positional_stack.pop() {
            self.positional_params = old;
        }
    }

    // Array helpers
    pub fn get_array_element(&self, name: &str, index: &str) -> Option<String> {
        if let Some(arr) = self.arrays.get(name) {
            let idx: usize = index.parse().ok()?;
            arr.get(idx).cloned()
        } else if let Some(map) = self.assoc_arrays.get(name) {
            map.get(index).cloned()
        } else {
            None
        }
    }

    pub fn set_array_element(&mut self, name: &str, index: &str, value: &str) {
        if self.assoc_arrays.contains_key(name) {
            self.assoc_arrays
                .get_mut(name)
                .unwrap()
                .insert(index.to_string(), value.to_string());
        } else {
            let arr = self.arrays.entry(name.to_string()).or_default();
            if let Ok(idx) = index.parse::<usize>() {
                if idx >= arr.len() {
                    arr.resize(idx + 1, String::new());
                }
                arr[idx] = value.to_string();
            }
        }
    }

    /// Non-blocking reap of finished process-substitution children, so they do
    /// not linger as zombies. Still-running children are kept for a later sweep.
    pub fn reap_procsubs(&mut self) {
        use nix::sys::wait::{waitpid, WaitPidFlag, WaitStatus};
        use nix::unistd::Pid;
        if self.procsub_pids.is_empty() {
            return;
        }
        self.procsub_pids.retain(|&pid| {
            matches!(
                waitpid(Pid::from_raw(pid), Some(WaitPidFlag::WNOHANG)),
                Ok(WaitStatus::StillAlive)
            )
        });
    }

    /// Replace an indexed array's contents wholesale.
    pub fn set_array(&mut self, name: &str, values: Vec<String>) {
        self.assoc_arrays.remove(name);
        self.arrays.insert(name.to_string(), values);
    }

    pub fn array_values(&self, name: &str) -> Vec<String> {
        if let Some(arr) = self.arrays.get(name) {
            arr.clone()
        } else if let Some(map) = self.assoc_arrays.get(name) {
            map.values().cloned().collect()
        } else {
            Vec::new()
        }
    }

    pub fn array_keys(&self, name: &str) -> Vec<String> {
        if let Some(arr) = self.arrays.get(name) {
            (0..arr.len()).map(|i| i.to_string()).collect()
        } else if let Some(map) = self.assoc_arrays.get(name) {
            map.keys().cloned().collect()
        } else {
            Vec::new()
        }
    }

    pub fn array_length(&self, name: &str) -> usize {
        if let Some(arr) = self.arrays.get(name) {
            arr.len()
        } else if let Some(map) = self.assoc_arrays.get(name) {
            map.len()
        } else {
            0
        }
    }

    pub fn is_array(&self, name: &str) -> bool {
        self.arrays.contains_key(name) || self.assoc_arrays.contains_key(name)
    }

    /// Push a new local variable scope (for function entry)
    pub fn push_local_scope(&mut self) {
        self.local_vars_stack.push(HashMap::new());
    }

    /// Pop the current local variable scope (for function exit)
    pub fn pop_local_scope(&mut self) {
        self.local_vars_stack.pop();
    }
}
