/// Main shell REPL loop.
use crate::builtins;
use crate::config;
use crate::editor::Editor;
use crate::environment::ShellState;
use crate::execution;
use crate::executor;
use crate::history::History;
use crate::hooks;
use crate::osc;
use crate::parser;
use crate::prompt;
use crate::session;
use crate::signal;
use nix::errno::Errno;
use nix::fcntl::{open, OFlag};
use nix::libc;
use nix::sys::stat::Mode;
use nix::unistd::{getpgrp, getpid, setpgid, tcsetpgrp};
use std::fs::File;
use std::io::{self, Read, Write};
use std::path::{Path, PathBuf};
use std::sync::atomic::Ordering;

pub struct Shell {
    pub state: ShellState,
    pub history: History,
    pub editor: Editor,
    pub session_id: Option<String>,
    /// True if a session snapshot was successfully restored (skip config loading).
    session_restored: bool,
    load_startup_config: bool,
    startup_file: Option<PathBuf>,
    execution_seq: u64,
}

pub(crate) const MAX_PROGRAM_BYTES: usize = 64 * 1024 * 1024;

fn execute_text(source: &str, state: &mut ShellState) -> i32 {
    match parser::parse(source) {
        Ok(commands) => executor::execute_program(&commands, state),
        Err(e) => {
            eprintln!("jsh: {}", e);
            state.last_exit_code = 2;
            2
        }
    }
}

fn finish_noninteractive(mut state: ShellState, status: i32) -> i32 {
    state.last_exit_code = status;
    run_exit_trap(&mut state)
}

fn noninteractive_state(arg0: &str, args: &[String]) -> ShellState {
    builtins::reset_exit_request();
    signal::reset_pending_signals();
    signal::install_noninteractive_signals();
    let mut state = ShellState::new(false);
    state.set_invocation(arg0, args);
    state
}

enum ProgramReadError {
    Signaled(i32),
    Io(io::Error),
}

/// Read a complete noninteractive program, waking promptly for INT/HUP/TERM.
/// Any bytes received before a terminating signal are intentionally discarded:
/// a shell must never execute a truncated program.
fn read_program_interruptibly(mut reader: impl Read) -> Result<String, ProgramReadError> {
    read_program_interruptibly_with_limit(&mut reader, MAX_PROGRAM_BYTES)
}

fn read_program_interruptibly_with_limit(
    mut reader: impl Read,
    max_bytes: usize,
) -> Result<String, ProgramReadError> {
    let mut input = Vec::new();
    let mut buffer = [0_u8; 8192];

    loop {
        if let Some(status) = signal::take_pending_status() {
            return Err(ProgramReadError::Signaled(status));
        }
        match reader.read(&mut buffer) {
            Ok(0) => break,
            Ok(read) => {
                if input.len().saturating_add(read) > max_bytes {
                    return Err(ProgramReadError::Io(io::Error::new(
                        io::ErrorKind::InvalidData,
                        format!("program exceeds the {max_bytes} byte limit"),
                    )));
                }
                input.extend_from_slice(&buffer[..read]);
            }
            Err(error) if error.kind() == io::ErrorKind::Interrupted => {
                if let Some(status) = signal::take_pending_status() {
                    return Err(ProgramReadError::Signaled(status));
                }
            }
            Err(error) => return Err(ProgramReadError::Io(error)),
        }
    }

    if let Some(status) = signal::take_pending_status() {
        return Err(ProgramReadError::Signaled(status));
    }
    String::from_utf8(input)
        .map_err(|error| ProgramReadError::Io(io::Error::new(io::ErrorKind::InvalidData, error)))
}

fn read_noninteractive_stdin() -> Result<String, ProgramReadError> {
    read_program_interruptibly(io::stdin().lock())
}

fn read_script_interruptibly(path: &Path) -> Result<String, ProgramReadError> {
    let file = loop {
        if let Some(status) = signal::take_pending_status() {
            return Err(ProgramReadError::Signaled(status));
        }
        match open(path, OFlag::O_RDONLY | OFlag::O_CLOEXEC, Mode::empty()) {
            Ok(fd) => break File::from(fd),
            Err(Errno::EINTR) => {
                if let Some(status) = signal::take_pending_status() {
                    return Err(ProgramReadError::Signaled(status));
                }
            }
            Err(error) => {
                return Err(ProgramReadError::Io(io::Error::from_raw_os_error(
                    error as i32,
                )));
            }
        }
    };
    read_program_interruptibly(file)
}

/// Execute a `-c` command without loading startup files, history, or sessions.
pub fn run_command(command: &str, arg0: &str, args: &[String]) -> i32 {
    let mut state = noninteractive_state(arg0, args);
    let status = execute_text(command, &mut state);
    finish_noninteractive(state, status)
}

/// Resolve the script named on the command line the way bash does: a bare name
/// (no slash) that is not a file in the cwd is looked up in `$PATH`. Bash only
/// needs the file to be readable there, not executable. Anything else — a
/// relative or absolute path — is opened exactly as given, and a name that
/// cannot be found anywhere is returned unchanged so the caller's diagnostic
/// still quotes what the user typed.
fn resolve_script_path(path: &Path) -> PathBuf {
    resolve_script_in(path, std::env::var("PATH").ok().as_deref())
}

/// `resolve_script_path` with the search path passed in, so tests do not have to
/// mutate the process environment.
fn resolve_script_in(path: &Path, search_path: Option<&str>) -> PathBuf {
    if path.to_string_lossy().contains('/') || path.is_file() {
        return path.to_path_buf();
    }
    for dir in search_path.unwrap_or("").split(':') {
        let dir = if dir.is_empty() { "." } else { dir };
        let candidate = Path::new(dir).join(path);
        if candidate.is_file() {
            return candidate;
        }
    }
    path.to_path_buf()
}

/// Execute a script file without loading startup files, history, or sessions.
pub fn run_script(path: &Path, args: &[String]) -> i32 {
    let arg0 = path.to_string_lossy().into_owned();
    let script_path = resolve_script_path(path);
    let mut state = noninteractive_state(&arg0, args);
    // Bash exposes the running script as `${BASH_SOURCE[0]}` whether it was
    // sourced or executed; jsh only set it while sourcing. With it unset,
    // `SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"` — the idiom
    // nearly every setup script opens with — saw `dirname ""`, resolved to the
    // *caller's* cwd, and silently read files from the wrong directory.
    state.set_array(
        "BASH_SOURCE",
        vec![script_path.to_string_lossy().into_owned()],
    );
    let status = match read_script_interruptibly(&script_path) {
        Ok(content) => {
            let source = match content.strip_prefix("#!") {
                Some(rest) => rest.split_once('\n').map_or("", |(_, body)| body),
                None => &content,
            };
            execute_text(source, &mut state)
        }
        Err(ProgramReadError::Signaled(status)) => {
            state.last_exit_code = status;
            status
        }
        Err(ProgramReadError::Io(e)) => {
            eprintln!("jsh: {}: {}", path.display(), e);
            let code = if e.kind() == std::io::ErrorKind::NotFound {
                127
            } else {
                126
            };
            state.last_exit_code = code;
            code
        }
    };
    finish_noninteractive(state, status)
}

/// Read and execute standard input without loading or writing user state.
pub fn run_stdin(arg0: &str, args: &[String]) -> i32 {
    let mut state = noninteractive_state(arg0, args);
    let status = match read_noninteractive_stdin() {
        Ok(source) => execute_text(&source, &mut state),
        Err(ProgramReadError::Signaled(status)) => {
            state.last_exit_code = status;
            status
        }
        Err(ProgramReadError::Io(error)) => {
            eprintln!("jsh: stdin: {}", error);
            state.last_exit_code = 1;
            1
        }
    };
    finish_noninteractive(state, status)
}

/// Run non-interactive modes (-c command, script file) without creating
/// Editor or History, avoiding fork overhead from `tput` / terminal queries.
pub fn run_noninteractive() -> Option<i32> {
    let args: Vec<String> = std::env::args().collect();

    if args.len() >= 3 && args[1] == "-c" {
        let cmd_str = &args[2];
        let (arg0, positional) = match args.get(3) {
            Some(arg0) => (arg0.as_str(), &args[4..]),
            None => (args[0].as_str(), &args[0..0]),
        };
        return Some(run_command(cmd_str, arg0, positional));
    }

    if args.len() >= 2 && !args[1].starts_with('-') {
        return Some(run_script(Path::new(&args[1]), &args[2..]));
    }

    None // interactive mode
}

/// Expand history references: !!, !$, !n, !-n
fn expand_history(line: &str, history: &crate::history::History) -> Option<String> {
    if !line.contains('!') {
        return Some(line.to_string());
    }

    let mut result = String::new();
    let mut chars = line.chars().peekable();
    let mut in_single_quote = false;

    while let Some(c) = chars.next() {
        if c == '\'' {
            in_single_quote = !in_single_quote;
            result.push(c);
            continue;
        }
        if in_single_quote || c != '!' {
            result.push(c);
            continue;
        }
        match chars.peek() {
            Some(&'!') => {
                chars.next();
                result.push_str(history.last()?);
            }
            Some(&'$') => {
                chars.next();
                let prev = history.last()?;
                let last_arg = prev.split_whitespace().last().unwrap_or(prev);
                result.push_str(last_arg);
            }
            Some(&c2) if c2.is_ascii_digit() => {
                let mut num_str = String::new();
                while let Some(&d) = chars.peek() {
                    if d.is_ascii_digit() {
                        num_str.push(d);
                        chars.next();
                    } else {
                        break;
                    }
                }
                let n: usize = num_str.parse().unwrap_or(0);
                if n == 0 {
                    return None;
                }
                result.push_str(history.get(n - 1)?);
            }
            Some(&'-') => {
                chars.next();
                let mut num_str = String::new();
                while let Some(&d) = chars.peek() {
                    if d.is_ascii_digit() {
                        num_str.push(d);
                        chars.next();
                    } else {
                        break;
                    }
                }
                let n: usize = num_str.parse().unwrap_or(0);
                if n == 0 || n > history.len() {
                    return None;
                }
                result.push_str(history.get(history.len() - n)?);
            }
            _ => {
                result.push('!');
            }
        }
    }
    Some(result)
}

fn run_exit_trap(state: &mut ShellState) -> i32 {
    let incoming_status = state.last_exit_code;
    let prior_exit_requested = builtins::EXIT_REQUESTED.swap(false, Ordering::SeqCst);
    let prior_exit_code = builtins::EXIT_CODE.load(Ordering::SeqCst);
    let prior_abort = std::mem::replace(&mut state.abort_current_program, false);
    if let Some(cmd) = state.traps.get("EXIT").cloned() {
        if let Ok(commands) = parser::parse(&cmd) {
            for c in &commands {
                executor::execute_complete_command(c, state);
                if builtins::EXIT_REQUESTED.load(Ordering::SeqCst)
                    || signal::pending_status().is_some()
                {
                    break;
                }
            }
        }
    }
    let trap_requested_exit = builtins::EXIT_REQUESTED.load(Ordering::SeqCst);
    let requested_status = if trap_requested_exit {
        builtins::EXIT_CODE.load(Ordering::SeqCst)
    } else if prior_exit_requested {
        prior_exit_code
    } else {
        incoming_status
    };
    // A terminating signal wins over the command or EXIT-trap status, including
    // when it arrived while an EXIT-trap child was in the foreground.
    let pending_status = signal::take_pending_status();
    let final_status = pending_status.unwrap_or(requested_status);
    builtins::EXIT_CODE.store(final_status, Ordering::SeqCst);
    builtins::EXIT_REQUESTED.store(
        prior_exit_requested || trap_requested_exit || pending_status.is_some(),
        Ordering::SeqCst,
    );
    state.abort_current_program = prior_abort;
    state.last_exit_code = final_status;
    final_status
}

impl Default for Shell {
    fn default() -> Self {
        Self::new()
    }
}

impl Shell {
    pub fn new() -> Self {
        Shell {
            state: ShellState::new(true),
            history: History::new(10000),
            editor: Editor::new(),
            session_id: None,
            session_restored: false,
            load_startup_config: true,
            startup_file: None,
            execution_seq: 0,
        }
    }

    /// Configure interactive startup-file loading.
    ///
    /// `rcfile` replaces the default `.bashrc`/`.jshrc` choice when present.
    pub fn configure_startup(&mut self, load_config: bool, rcfile: Option<PathBuf>) {
        self.load_startup_config = load_config;
        self.startup_file = rcfile;
    }

    /// Restore session state from a snapshot file.
    /// Always sets session_id so that save_session() works on exit,
    /// even if no prior snapshot exists (first launch).
    pub fn restore_session(&mut self, session_id: &str) {
        self.session_id = Some(session_id.to_string());

        match session::SessionSnapshot::load(session_id) {
            Ok(snapshot) => {
                let ctx = snapshot.environment_context.clone();
                snapshot.apply(&mut self.state);
                session::reactivate_environment(&ctx, &mut self.state);
                self.session_restored = true;
            }
            Err(error) => {
                // A missing snapshot is normal on first launch. Corrupt,
                // unsupported, or insecure snapshots need a visible diagnostic.
                let not_found = error
                    .downcast_ref::<std::io::Error>()
                    .is_some_and(|io_error| io_error.kind() == std::io::ErrorKind::NotFound);
                if !not_found {
                    eprintln!("jsh: session restore: {error}");
                }
            }
        }
    }

    /// Whether a session snapshot was successfully loaded (controls config skip).
    pub fn was_restored(&self) -> bool {
        self.session_restored
    }

    /// Save session state to disk.
    fn save_session(&self) {
        if let Some(ref id) = self.session_id {
            let snapshot = session::SessionSnapshot::capture(&self.state, id);
            if let Err(e) = snapshot.save() {
                eprintln!("jsh: failed to save session: {}", e);
            }
        }
    }

    pub fn run(&mut self) -> i32 {
        builtins::reset_exit_request();
        signal::reset_pending_signals();
        signal::install_shell_signals();

        // Check if stdin is a TTY for interactive mode
        // Use libc::isatty directly to catch deleted/invalid ptys that a
        // high-level terminal check might miss.
        let stdin_is_tty = unsafe { libc::isatty(libc::STDIN_FILENO) == 1 };

        // Update interactive mode based on stdin
        self.state.interactive = stdin_is_tty;

        if stdin_is_tty {
            // jsh draws its own prompt and never reads PS1, but every rc ever
            // written tests it — `[ -z "$PS1" ] && return` is how a startup
            // file decides it is talking to an interactive shell. Leaving it
            // unset makes jsh look non-interactive to the file it is about to
            // read, so the aliases and settings below that line never load. A
            // user's own PS1 assignment overwrites this, as it does in bash.
            if self.state.get_var("PS1").is_none() {
                self.state.set_var("PS1", "$ ");
            }
            // Only interactive shells load startup configuration. A restored
            // snapshot already contains the accumulated startup state.
            if self.load_startup_config && !self.session_restored {
                if let Some(path) = self.startup_file.as_deref() {
                    config::load_config_file(path, &mut self.state);
                } else {
                    config::load_config(&mut self.state);
                }
            }
            if builtins::EXIT_REQUESTED.load(Ordering::SeqCst) {
                self.state.last_exit_code = builtins::EXIT_CODE.load(Ordering::SeqCst);
                return self.finish_interactive();
            }
            config::refresh_shell_integrations(&mut self.state);
            if builtins::EXIT_REQUESTED.load(Ordering::SeqCst) {
                self.state.last_exit_code = builtins::EXIT_CODE.load(Ordering::SeqCst);
                return self.finish_interactive();
            }
            init_interactive_job_control();
            // Interactive mode with editor
            self.run_interactive()
        } else {
            // Defensive fallback for legacy callers. The CLI normally calls
            // `run_stdin` directly, avoiding Editor/History construction too.
            signal::install_noninteractive_signals();
            self.run_from_stdin()
        }
    }

    fn run_interactive(&mut self) -> i32 {
        // Report session ID to the terminal emulator via OSC 7770
        if let Some(ref id) = self.session_id {
            osc::report_session_id(id);
        }

        // Initial OSC emissions so the terminal knows CWD at startup
        osc::report_cwd(&self.state.hostname);
        osc::report_cwd_iterm2();

        loop {
            // Check background jobs
            self.state.jobs.check_background();
            self.state
                .jobs
                .notify_done_with_notification(self.state.notification_threshold);

            // Run precmd hooks
            let precmd = self.state.hooks.precmd.clone();
            hooks::run_hooks(&precmd, &mut self.state);

            // OSC 2 — set window title to current directory
            {
                let title = prompt::get_short_cwd(&self.state);
                osc::set_title(&format!("jsh: {}", title));
            }

            // Probe Git once per prompt. Both prompt rendering and smart suggestions
            // consume this cache, so commands such as `git commit` do not trigger a
            // second Git lookup while the next prompt is being drawn.
            let git = prompt::probe_git_context();
            self.state.cached_git_branch = git.branch;
            self.state.cached_git_remote = git.remote;
            self.state.cached_git_has_staged = git.has_staged;
            self.state.cached_git_has_unstaged = git.has_unstaged;
            self.state.cached_git_has_conflicts = git.has_conflicts;
            self.state.cached_git_ahead = git.ahead;
            self.state.cached_git_behind = git.behind;

            match self.editor.read_line(&mut self.state, &mut self.history) {
                Ok(Some(line)) => {
                    let line = line.trim().to_string();
                    if line.is_empty() {
                        continue;
                    }

                    // History expansion
                    let line = match expand_history(&line, &self.history) {
                        Some(expanded) => {
                            if expanded != line {
                                eprintln!("{}", expanded);
                            }
                            expanded
                        }
                        None => {
                            eprintln!("jsh: !: event not found");
                            continue;
                        }
                    };

                    self.history.add_with_cwd(
                        &line,
                        std::env::current_dir()
                            .ok()
                            .as_ref()
                            .map(|p| p.to_string_lossy().as_ref().to_string())
                            .as_deref(),
                    );
                    self.state.last_command = Some(line.clone());

                    // Run preexec hooks
                    let preexec = self.state.hooks.preexec.clone();
                    hooks::run_hooks(&preexec, &mut self.state);

                    // OSC 2 — set window title to the running command
                    osc::set_title(&line);

                    // Assign every accepted interactive command a stable ID shared by
                    // OSC metadata, the execution journal, and AI error context.
                    self.execution_seq = self.execution_seq.wrapping_add(1);
                    if self.execution_seq == 0 {
                        self.execution_seq = 1;
                    }
                    let execution_id =
                        execution::execution_id(self.session_id.as_deref(), self.execution_seq);
                    let cwd_before = std::env::current_dir()
                        .map(|path| path.to_string_lossy().into_owned())
                        .unwrap_or_default();
                    let started_at_ms = execution::unix_time_ms();
                    let journal = execution::ExecutionJournal::configured();
                    let journal_started = journal.as_ref().is_some_and(|journal| {
                        journal
                            .record_start(
                                &execution_id,
                                self.session_id.as_deref(),
                                self.execution_seq,
                                &line,
                                &cwd_before,
                                started_at_ms,
                            )
                            .is_ok()
                    });

                    // OSC 133;C — command output start
                    osc::command_output_start(&execution_id, &line, &cwd_before);

                    // Parse and execute
                    let cmd_start = std::time::Instant::now();
                    match parser::parse(&line) {
                        Ok(commands) => {
                            executor::execute_program(&commands, &mut self.state);
                        }
                        Err(e) => {
                            eprintln!("jsh: {}", e);
                            self.state.last_exit_code = 2;
                        }
                    }
                    let command_duration = cmd_start.elapsed();
                    self.state.last_command_duration = Some(command_duration);
                    let duration_ms = command_duration.as_millis().min(u128::from(u64::MAX)) as u64;

                    // Files, variables, Git state, PATH and CWD may all have
                    // changed. Never carry dynamic Tab results across commands.
                    crate::completer::clear_cache();

                    // Capture error info for AI fix suggestions
                    if self.state.last_exit_code != 0 {
                        self.editor.last_error_info = Some((
                            line.clone(),
                            format!("exit code {}", self.state.last_exit_code),
                            self.state.last_exit_code,
                        ));
                        self.editor.last_error_execution_id = Some(execution_id.clone());
                    } else {
                        self.editor.last_error_info = None;
                        self.editor.last_error_execution_id = None;
                    }

                    let cwd_after = std::env::current_dir()
                        .map(|path| path.to_string_lossy().into_owned())
                        .unwrap_or_default();
                    let ended_at_ms = execution::unix_time_ms();

                    // OSC 133;D must reach the terminal before the journal finish
                    // event is appended, so the terminal can close and capture the
                    // rendered output range associated with this execution ID.
                    osc::command_finished(
                        self.state.last_exit_code,
                        &execution_id,
                        duration_ms,
                        &cwd_after,
                    );
                    if journal_started {
                        if let Some(journal) = journal.as_ref() {
                            let _ = journal.record_finish(
                                &execution_id,
                                self.state.last_exit_code,
                                duration_ms,
                                &cwd_after,
                                ended_at_ms,
                            );
                        }
                    }

                    // Check if `exit` builtin was called
                    if builtins::EXIT_REQUESTED.load(Ordering::SeqCst) {
                        self.state.last_exit_code = builtins::EXIT_CODE.load(Ordering::SeqCst);
                        break;
                    }
                }
                Ok(None) => {
                    break;
                }
                Err(e) => {
                    // A read_line error usually means the terminal/pty was torn down
                    // (e.g. EIO once the master is closed). Don't use eprintln! here:
                    // writing to a dead stderr fails and eprintln! would panic, which
                    // would skip the session/history save below. Report best-effort.
                    let _ = writeln!(io::stderr(), "jsh: editor error: {}", e);
                    break;
                }
            }
        }

        self.finish_interactive()
    }

    fn finish_interactive(&mut self) -> i32 {
        let mut final_status = run_exit_trap(&mut self.state);
        self.save_session();
        self.history.save();

        // Opportunistically clean up stale session files (older than 7 days)
        session::cleanup_stale_sessions(std::time::Duration::from_secs(7 * 24 * 3600));
        if let Some(signal_status) = signal::take_pending_status() {
            final_status = signal_status;
            self.state.last_exit_code = signal_status;
            builtins::EXIT_CODE.store(signal_status, Ordering::SeqCst);
            builtins::EXIT_REQUESTED.store(true, Ordering::SeqCst);
        }
        final_status
    }

    fn run_from_stdin(&mut self) -> i32 {
        // Non-interactive stdin commonly contains shell scripts and hook payloads
        // with multi-line function definitions. Parse the whole buffer at once.
        let script = match read_noninteractive_stdin() {
            Ok(script) => script,
            Err(ProgramReadError::Signaled(status)) => {
                self.state.last_exit_code = status;
                return run_exit_trap(&mut self.state);
            }
            Err(ProgramReadError::Io(error)) => {
                eprintln!("jsh: stdin: {}", error);
                self.state.last_exit_code = 1;
                return run_exit_trap(&mut self.state);
            }
        };

        if script.trim().is_empty() {
            return run_exit_trap(&mut self.state);
        }

        let preexec = self.state.hooks.preexec.clone();
        hooks::run_hooks(&preexec, &mut self.state);

        let cmd_start = std::time::Instant::now();
        match parser::parse(&script) {
            Ok(commands) => {
                executor::execute_program(&commands, &mut self.state);
            }
            Err(e) => {
                eprintln!("jsh: {}", e);
                self.state.last_exit_code = 2;
            }
        }
        self.state.last_command_duration = Some(cmd_start.elapsed());

        run_exit_trap(&mut self.state)
    }
}

fn init_interactive_job_control() {
    let shell_pid = getpid();

    // Interactive shells need their own process group before handing the
    // terminal to child jobs and reclaiming it later.
    setpgid(shell_pid, shell_pid).ok();

    let shell_pgid = getpgrp();
    tcsetpgrp(std::io::stdin(), shell_pgid).ok();
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Bash searches `$PATH` for a bare script name and reports the file it
    /// actually opened as `${BASH_SOURCE[0]}`; a name with a slash, or one that
    /// exists in the cwd, is never searched for.
    #[test]
    fn script_path_is_searched_in_path_only_for_a_bare_name() {
        let dir = tempfile::tempdir().expect("tempdir");
        let bin = dir.path().join("bin");
        std::fs::create_dir(&bin).expect("mkdir bin");
        std::fs::write(bin.join("onpath.sh"), "true\n").expect("write script");

        let search = bin.to_str().unwrap();
        assert_eq!(
            resolve_script_in(Path::new("onpath.sh"), Some(search)),
            bin.join("onpath.sh")
        );
        // A slash means "use this path", even when the basename is on PATH.
        assert_eq!(
            resolve_script_in(Path::new("./onpath.sh"), Some(search)),
            PathBuf::from("./onpath.sh")
        );
        // A name found nowhere is returned unchanged so the diagnostic can still
        // quote what the user typed.
        assert_eq!(
            resolve_script_in(Path::new("missing.sh"), Some(search)),
            PathBuf::from("missing.sh")
        );
        assert_eq!(
            resolve_script_in(Path::new("onpath.sh"), None),
            PathBuf::from("onpath.sh")
        );
    }

    #[test]
    fn complete_program_reads_are_bounded_before_parsing() {
        let error = read_program_interruptibly_with_limit(&b"12345"[..], 4).unwrap_err();
        assert!(matches!(error, ProgramReadError::Io(_)));
    }
}
