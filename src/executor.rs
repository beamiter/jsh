/// AST executor: fork/exec, pipes, redirects, compound commands.
use crate::builtins;
use crate::environment::ShellState;
use crate::expand::{expand_word, expand_word_to_string, expand_words};
use crate::parser::ast::*;
use crate::signal;

use nix::errno::Errno;
use nix::sys::wait::{waitpid, WaitPidFlag, WaitStatus};
use nix::unistd::{close, execvp, fork, pipe, setpgid, tcsetpgrp, ForkResult, Pid};
use std::cell::RefCell;
use std::collections::HashSet;
use std::ffi::CString;
use std::fs::{File, OpenOptions};
use std::io::{self, IsTerminal, Write};
use std::os::unix::fs::OpenOptionsExt;
use std::os::unix::io::{AsRawFd, BorrowedFd, FromRawFd, IntoRawFd, OwnedFd, RawFd};
use std::rc::{Rc, Weak};

fn shell_error(msg: &str) {
    eprintln!("{}", format_shell_error(msg, stderr_supports_color()));
}

fn shell_command_hint(suggestion: &str) {
    eprintln!(
        "{}",
        format_command_hint(suggestion, stderr_supports_color())
    );
}

fn stderr_supports_color() -> bool {
    should_use_color(
        std::io::stderr().is_terminal(),
        std::env::var_os("NO_COLOR").is_some(),
    )
}

fn should_use_color(is_terminal: bool, no_color_is_set: bool) -> bool {
    is_terminal && !no_color_is_set
}

fn format_shell_error(msg: &str, color: bool) -> String {
    if color {
        format!("\x1b[1;31mjsh:\x1b[0m {}", msg)
    } else {
        format!("jsh: {}", msg)
    }
}

fn format_command_hint(suggestion: &str, color: bool) -> String {
    let hint = format!("       did you mean '{suggestion}'?");
    if color {
        format!("\x1b[2;33m{}\x1b[0m", hint)
    } else {
        hint
    }
}

pub fn suggest_command(cmd: &str, state: &mut ShellState) -> Option<String> {
    let mut best: Option<(String, usize)> = None;
    let consider = |best: &mut Option<(String, usize)>, candidate: &str| {
        let dist = edit_distance(cmd, candidate);
        if dist == 0 || dist > 2 || dist >= cmd.len() {
            return;
        }
        match best {
            Some((_, d)) if dist < *d => *best = Some((candidate.to_string(), dist)),
            None => *best = Some((candidate.to_string(), dist)),
            _ => {}
        }
    };
    let cache = state.path_cache().clone();
    for candidate in cache.iter() {
        consider(&mut best, candidate);
    }
    for name in crate::command_catalog::builtin_names() {
        consider(&mut best, name);
    }
    // Phase 15d: include user-defined `def` functions, aliases, and shell functions.
    for name in state.user_signatures.keys() {
        consider(&mut best, name);
    }
    for name in state.aliases.keys() {
        consider(&mut best, name);
    }
    for name in state.functions.keys() {
        consider(&mut best, name);
    }
    best.map(|(s, _)| s)
}

fn edit_distance(a: &str, b: &str) -> usize {
    let a: Vec<char> = a.chars().collect();
    let b: Vec<char> = b.chars().collect();
    let (m, n) = (a.len(), b.len());
    if m == 0 {
        return n;
    }
    if n == 0 {
        return m;
    }
    let mut prev: Vec<usize> = (0..=n).collect();
    let mut curr = vec![0; n + 1];
    for i in 1..=m {
        curr[0] = i;
        for j in 1..=n {
            let cost = if a[i - 1] == b[j - 1] { 0 } else { 1 };
            curr[j] = (prev[j] + 1).min(curr[j - 1] + 1).min(prev[j - 1] + cost);
        }
        std::mem::swap(&mut prev, &mut curr);
    }
    prev[n]
}

/// Give terminal foreground to `pgrp`, then wait for the process, then reclaim
/// the terminal for the shell's own process group.
fn wait_for_fg(pid: Pid, state: &mut ShellState) -> i32 {
    let shell_pgid = nix::unistd::getpgrp();
    tcsetpgrp(std::io::stdin(), pid).ok();

    let status = match waitpid(pid, Some(WaitPidFlag::WUNTRACED)) {
        Ok(WaitStatus::Exited(_, code)) => code,
        Ok(WaitStatus::Signaled(_, sig, _)) => 128 + sig as i32,
        Ok(WaitStatus::Stopped(_, sig)) => {
            let cmd_str = format!("(pid {})", pid);
            let jid = state.jobs.add(pid, cmd_str.clone());
            if let Some(job) = state.jobs.get_by_id(jid) {
                job.status = crate::job::JobStatus::Stopped;
                eprintln!("\n[{}]+  Stopped                    {}", jid, cmd_str);
            }
            128 + sig as i32
        }
        _ => 1,
    };

    tcsetpgrp(std::io::stdin(), shell_pgid).ok();
    status
}

fn child_exit(code: i32) -> ! {
    let _ = std::io::stdout().flush();
    let _ = std::io::stderr().flush();
    unsafe { nix::libc::_exit(code) }
}

fn exec_error_info(_cmd_name: &str) -> (&'static str, i32) {
    match Errno::last() {
        Errno::EACCES => ("Permission denied", 126),
        Errno::ENOEXEC => ("cannot execute binary file", 126),
        Errno::ENOENT => ("command not found", 127),
        _ => ("command not found", 127),
    }
}

pub fn execute_program(commands: &[CompleteCommand], state: &mut ShellState) -> i32 {
    // A parsed command line/script is the lifetime of a fatal expansion
    // failure. Keep the flag set while unwinding nested compound lists, then
    // allow the next independently parsed program to run.
    state.abort_current_program = false;
    state.errexit_status = None;
    state.last_failure_exempt = false;
    if exit_requested() {
        let status = builtins::EXIT_CODE.load(std::sync::atomic::Ordering::SeqCst);
        state.last_exit_code = status;
        return status;
    }
    if let Some(status) = signal::take_pending_status() {
        state.last_exit_code = status;
        return status;
    }
    let mut last = 0;
    for cmd in commands {
        if let Some(status) = signal::take_pending_status() {
            last = status;
            state.last_exit_code = status;
            break;
        }
        let outcome = execute_complete_command_outcome(cmd, state);
        last = outcome.status;

        if let Some(signal_status) = signal::take_pending_status() {
            last = signal_status;
            state.last_exit_code = last;
            break;
        }

        if exit_requested() {
            last = builtins::EXIT_CODE.load(std::sync::atomic::Ordering::SeqCst);
            state.last_exit_code = last;
            break;
        }

        if state.abort_current_program {
            state.last_exit_code = last;
            break;
        }

        // A nested list already handled this failure and its ERR trap. Keep
        // unwinding rather than running another iteration or firing it twice.
        if let Some(status) = state.errexit_status {
            last = status;
            state.last_exit_code = last;
            break;
        }

        if last != 0 && !outcome.failure_exempt && state.errexit_suppression_depth == 0 {
            fire_err_trap(state);
            state.last_failure_exempt = outcome.failure_exempt;
        }

        if let Some(signal_status) = signal::take_pending_status() {
            last = signal_status;
            state.last_exit_code = last;
            break;
        }
        if exit_requested() {
            last = builtins::EXIT_CODE.load(std::sync::atomic::Ordering::SeqCst);
            state.last_exit_code = last;
            break;
        }
        if control_flow_requested(state) {
            break;
        }
        if errexit_active(state) && last != 0 && !outcome.failure_exempt {
            state.errexit_status = Some(last);
            break;
        }
    }
    state.last_exit_code = last;
    last
}

fn fire_err_trap(state: &mut ShellState) {
    if let Some(action) = state.traps.get("ERR").cloned() {
        if !action.is_empty() {
            if let Ok(cmds) = crate::parser::parse(&action) {
                let failed_status = state.last_exit_code;
                for cmd in &cmds {
                    execute_complete_command(cmd, state);
                    if exit_requested() || signal::pending_status().is_some() {
                        break;
                    }
                }
                if !exit_requested() && signal::pending_status().is_none() {
                    state.last_exit_code = failed_status;
                }
            }
        }
    }
}

#[derive(Clone, Copy, Debug)]
struct CommandOutcome {
    status: i32,
    /// A non-zero result produced in a shell conditional context (`&&`, `||`,
    /// or `!`) does not trigger ERR/errexit.
    failure_exempt: bool,
}

fn exit_requested() -> bool {
    builtins::EXIT_REQUESTED.load(std::sync::atomic::Ordering::SeqCst)
}

fn control_flow_requested(state: &ShellState) -> bool {
    execution_unwind_requested(state) || state.loop_break || state.loop_continue
}

fn execution_unwind_requested(state: &ShellState) -> bool {
    exit_requested()
        || signal::pending_status().is_some()
        || state.abort_current_program
        || state.errexit_status.is_some()
        || state.return_requested
}

fn errexit_active(state: &ShellState) -> bool {
    state.shell_opts.errexit && state.errexit_suppression_depth == 0
}

fn with_errexit_suppressed<F>(state: &mut ShellState, f: F) -> i32
where
    F: FnOnce(&mut ShellState) -> i32,
{
    state.errexit_suppression_depth += 1;
    let result = f(state);
    state.errexit_suppression_depth -= 1;
    result
}

fn execute_pipeline_in_context(
    pipeline: &Pipeline,
    state: &mut ShellState,
    conditional: bool,
) -> i32 {
    if conditional {
        with_errexit_suppressed(state, |state| execute_pipeline(pipeline, state))
    } else {
        execute_pipeline(pipeline, state)
    }
}

/// `set -u`: report an unset parameter and abort, as bash does.
///
/// Bash treats this as a fatal expansion error: the message goes to stderr, the
/// status is 1 and a non-interactive shell stops right there. `abort_current_program`
/// is the same lever failglob already pulls, so nested lists unwind too.
fn report_nounset(state: &mut ShellState, param: &str) -> i32 {
    let message = format!("{}: unbound variable", param);
    eprintln!("jsh: {}", message);
    state.set_error(message, 1);
    state.last_exit_code = 1;
    if !state.interactive {
        state.abort_current_program = true;
    }
    1
}

/// The first parameter reference in `words` that `set -u` should reject, spelled
/// the way bash spells it in the message (`x` for a variable, `$1` for a
/// positional parameter).
///
/// Only the forms where bash is unambiguous are checked: a bare `$name`,
/// `${name}` or `$N`. `${x:-default}`, `${x+set}`, `${#x}`, `$@`/`$*` and `$?`
/// never trip it — the first two because bash exempts them, the rest because
/// the exhaustive check belongs where the parameter is actually expanded.
fn nounset_violation(words: &[Word], state: &ShellState) -> Option<String> {
    fn scan(parts: &[WordPart], state: &ShellState) -> Option<String> {
        for part in parts {
            let found = match part {
                WordPart::Variable(name) => unset_parameter(name, state),
                WordPart::DoubleQuoted(inner) => scan(inner, state),
                WordPart::BraceExpansion(alts) => alts.iter().find_map(|a| scan(a, state)),
                // Command substitutions, arithmetic and process substitutions
                // run their own expansion (in a child, for the first two), so
                // they check themselves.
                _ => None,
            };
            if found.is_some() {
                return found;
            }
        }
        None
    }
    words.iter().find_map(|w| scan(w, state))
}

pub(crate) fn unset_parameter(name: &str, state: &ShellState) -> Option<String> {
    let first = name.chars().next()?;
    if first.is_ascii_digit() {
        // `$0` is always set; `$12` is unset when there are fewer than 12 args.
        if !name.chars().all(|c| c.is_ascii_digit()) {
            return None;
        }
        let idx: usize = name.parse().unwrap_or(0);
        if idx > 0 && idx > state.positional_params.len() {
            return Some(format!("${}", name));
        }
        return None;
    }
    // Anything that is not a plain identifier is either a special parameter
    // (`@`, `*`, `?`, `-`, `#`) or carries an operator (`x:-y`, `#x`, `!x`).
    if !(first == '_' || first.is_ascii_alphabetic())
        || !name.chars().all(|c| c == '_' || c.is_ascii_alphanumeric())
    {
        return None;
    }
    if state.get_var(name).is_some()
        || state.is_array(name)
        || state.let_vars.contains_key(name)
        || state.arrays.contains_key(name)
    {
        return None;
    }
    Some(name.to_string())
}

/// `$PS4` (default `+ `), the xtrace prefix.
fn xtrace_prefix(state: &ShellState) -> String {
    state
        .get_var("PS4")
        .map(|s| s.to_string())
        .unwrap_or_else(|| "+ ".to_string())
}

/// Quote a traced word the way bash's xtrace does, so a word with spaces or
/// shell metacharacters reads back as one word.
fn xtrace_quote(word: &str) -> String {
    let needs_quotes = word.is_empty()
        || word
            .chars()
            .any(|c| c.is_whitespace() || "\"'$&|;<>()*?[]{}~#!^`\\".contains(c) || c.is_control());
    if needs_quotes {
        format!("'{}'", word.replace('\'', "'\\''"))
    } else {
        word.to_string()
    }
}

fn xtrace_line(state: &ShellState, words: &[String]) {
    let line: Vec<String> = words.iter().map(|w| xtrace_quote(w)).collect();
    eprintln!("{}{}", xtrace_prefix(state), line.join(" "));
}

/// Evaluate a `((...))` command or one clause of a `for ((...))` header.
///
/// A malformed or failing expression reports `((: ...` and yields status 1,
/// and the script carries on, as in bash. A failure while *expanding* the text
/// (`(( $((1/0)) ))`, `${x:?}`) is an ordinary expansion error, which aborts.
fn run_arith_clause(expr: &str, state: &mut ShellState) -> Result<i64, i32> {
    let Some(text) = crate::expand::expand_arithmetic_text(expr, state) else {
        return Err(report_expansion_error(state).unwrap_or(1));
    };
    if state.shell_opts.xtrace {
        eprintln!("{}(( {text} ))", xtrace_prefix(state));
    }
    crate::expand::evaluate_arithmetic_text(&text, state).map_err(|message| {
        // An unbound variable under `set -u` comes back as an expansion
        // error, which is fatal here too.
        if let Some(code) = report_expansion_error(state) {
            return code;
        }
        eprintln!("jsh: ((: {message}");
        1
    })
}

fn report_expansion_error(state: &mut ShellState) -> Option<i32> {
    state.take_expansion_error().map(|message| {
        eprintln!("jsh: {}", message);
        state.set_error(message, 1);
        state.last_exit_code = 1;
        state.abort_current_program = true;
        1
    })
}

pub fn execute_complete_command(cmd: &CompleteCommand, state: &mut ShellState) -> i32 {
    execute_complete_command_outcome(cmd, state).status
}

fn execute_complete_command_outcome(
    cmd: &CompleteCommand,
    state: &mut ShellState,
) -> CommandOutcome {
    // Sweep up any finished process-substitution children (non-blocking).
    state.reap_procsubs();
    // Phase 5b: drop inline-closure stash from the prior top-level command.
    state.inline_closures.clear();
    if cmd.background {
        // Fork for background execution
        match unsafe { fork() } {
            Ok(ForkResult::Child) => {
                signal::reset_child_signals();
                let pid = nix::unistd::getpid();
                setpgid(pid, pid).ok();
                let code = execute_and_or(&cmd.list, state).status;
                child_exit(code);
            }
            Ok(ForkResult::Parent { child }) => {
                setpgid(child, child).ok();
                state.last_bg_pid = Some(child.as_raw() as u32);
                if !cmd.disown {
                    let cmd_str = "(background)";
                    let jid = state.jobs.add(child, cmd_str.to_string());
                    eprintln!("[{}] {}", jid, child);
                }
                state.last_exit_code = 0;
                return CommandOutcome {
                    status: 0,
                    failure_exempt: true,
                };
            }
            Err(e) => {
                eprintln!("jsh: fork failed: {}", e);
                state.last_exit_code = 1;
                return CommandOutcome {
                    status: 1,
                    failure_exempt: false,
                };
            }
        }
    }

    let outcome = execute_and_or(&cmd.list, state);
    state.last_exit_code = outcome.status;
    state.last_failure_exempt = outcome.failure_exempt;
    outcome
}

fn execute_and_or(list: &AndOrList, state: &mut ShellState) -> CommandOutcome {
    let first_is_conditional = list.first.negated || !list.rest.is_empty();
    let mut code = execute_pipeline_in_context(&list.first, state, first_is_conditional);
    let mut inherited_exemption = state.last_failure_exempt;
    // `$?` is observable while expanding the next pipeline in the same
    // AND-OR list, not only after the complete command finishes.
    state.last_exit_code = code;
    let mut last_executed = 0usize;

    for (index, (conn, pipeline)) in list.rest.iter().enumerate() {
        if control_flow_requested(state) {
            break;
        }
        let execute = match conn {
            Connector::And => code == 0,
            Connector::Or => code != 0,
        };
        if execute {
            let has_following_connector = index + 1 < list.rest.len();
            let conditional = pipeline.negated || has_following_connector;
            code = execute_pipeline_in_context(pipeline, state, conditional);
            inherited_exemption = state.last_failure_exempt;
            state.last_exit_code = code;
            last_executed = index + 1;
        }
    }

    let final_pipeline = if last_executed == 0 {
        &list.first
    } else {
        &list.rest[last_executed - 1].1
    };
    CommandOutcome {
        status: code,
        failure_exempt: final_pipeline.negated
            || last_executed < list.rest.len()
            || inherited_exemption,
    }
}

/// True if `cmd` is a `Simple` command whose head literal names a value-aware
/// builtin AND has no redirects / assignments. Conservative: any expansion
/// in the command name disqualifies (we don't want to side-effect expansion).
fn is_value_aware_command(cmd: &Command) -> bool {
    let simple = match cmd {
        Command::Simple(s) => s,
        _ => return false,
    };
    if !simple.redirects.is_empty() || !simple.assignments.is_empty() {
        return false;
    }
    let first = match simple.words.first() {
        Some(w) => w,
        None => return false,
    };
    // Only accept the trivial case: a single literal word part.
    if first.len() != 1 {
        return false;
    }
    let name = match &first[0] {
        WordPart::Literal(s) => s.as_str(),
        _ => return false,
    };
    crate::value_builtins::is_value_aware_in_pipeline(name)
}

/// Run a pipeline composed entirely of value-aware builtins in-process.
fn execute_value_pipeline(cmds: &[Command], state: &mut ShellState) -> i32 {
    use crate::pipeline_data::PipelineData;
    const MAX_VALUE_PIPE_INPUT_BYTES: usize = 64 * 1024 * 1024;
    crate::pipeline_data::clear_stream_failure();

    // If our own stdin is a pipe (not a tty), the user is feeding bytes into
    // the first value-aware stage. Read them eagerly. (Phase 5a is non-streaming.)
    // Skip when the first command is a source (open / ls / ps) that ignores stdin.
    let first_is_source = matches!(
        cmds.first()
            .and_then(|c| match c {
                Command::Simple(s) => s.words.first(),
                _ => None,
            })
            .and_then(|w| w.first())
            .and_then(|p| match p {
                WordPart::Literal(s) => Some(s.as_str()),
                _ => None,
            }),
        Some("open" | "ls" | "ps")
    );
    let mut data = if first_is_source || std::io::stdin().is_terminal() {
        PipelineData::Empty
    } else {
        let buf = match crate::io_guard::read_to_end_bounded(
            std::io::stdin().lock(),
            MAX_VALUE_PIPE_INPUT_BYTES,
        ) {
            Ok(bytes) => bytes,
            Err(error) => {
                eprintln!("jsh: value pipeline stdin: {error}");
                state.last_exit_code = 1;
                return 1;
            }
        };
        if buf.is_empty() {
            PipelineData::Empty
        } else {
            PipelineData::Bytes(buf)
        }
    };
    for (i, cmd) in cmds.iter().enumerate() {
        let simple = match cmd {
            Command::Simple(s) => s,
            _ => unreachable!("is_value_aware_command gate"),
        };
        // Expand the args (the head is known to be a literal, so expand_words is fine).
        let expanded = expand_words(&simple.words, state);
        if let Some(code) = report_expansion_error(state) {
            return code;
        }
        if expanded.is_empty() {
            continue;
        }
        let name = &expanded[0];
        let args = &expanded[1..];
        let f = match crate::value_builtins::VALUE_BUILTINS.get(name.as_str()) {
            Some(f) => *f,
            None => {
                eprintln!("jsh: {}: not value-aware (shouldn't happen)", name);
                return 1;
            }
        };
        if let Some(sig) = crate::signature::SIGNATURES.get(name.as_str()) {
            if let Err(msg) = sig.validate_args(args) {
                eprintln!("jsh: {}", msg);
                state.set_error(msg, 2);
                return 2;
            }
        }
        match f(data, args, state) {
            Ok(out) => data = out,
            Err(code) => {
                // A lazy upstream stage that failed explains why this stage
                // saw a truncated input; nothing else has reported it.
                if let Some(message) = crate::pipeline_data::take_stream_failure() {
                    eprintln!("jsh: {message}");
                    state.set_error(message, code);
                }
                return code;
            }
        }
        // Last command should print to stdout
        if i == cmds.len() - 1 {
            if let Err(e) = data.write_to_stdout() {
                eprintln!("jsh: {}", e);
                return 1;
            }
            data = PipelineData::Empty;
        }
    }
    0
}

/// Job control is active when the shell is interactive and `monitor` (`-m`) is
/// on. Absent `monitor` tracks the interactive default (on for interactive).
fn job_control_active(state: &ShellState) -> bool {
    state.interactive
        && state
            .shell_opts
            .tracked_opts
            .get("monitor")
            .copied()
            .unwrap_or(state.interactive)
}

fn execute_pipeline(pipeline: &Pipeline, state: &mut ShellState) -> i32 {
    state.last_failure_exempt = false;
    let cmds = &pipeline.commands;

    if cmds.len() == 1 {
        let code = execute_command(&cmds[0], state);
        state.pipestatus = vec![code];
        state.set_array("PIPESTATUS", vec![code.to_string()]);
        return if pipeline.negated {
            if code == 0 {
                1
            } else {
                0
            }
        } else {
            code
        };
    }

    // Phase 5a: if every command is a value-aware builtin with no redirects /
    // assignments / etc., run the whole pipeline in-process without forking.
    // A stage that would run the system program of the same name (`date +%s |
    // ...`) needs a process, so such a pipeline takes the forked route.
    if cmds.iter().all(is_value_aware_command)
        && crate::shadowing::value_pipeline_runs_builtins(cmds, state)
    {
        let code = execute_value_pipeline(cmds, state);
        state.pipestatus = vec![code];
        state.set_array("PIPESTATUS", vec![code.to_string()]);
        return if pipeline.negated {
            if code == 0 {
                1
            } else {
                0
            }
        } else {
            code
        };
    }

    // Bash `shopt -s lastpipe`: run the last stage in the current shell when
    // job control is not active (so side effects like `read` persist).
    let use_lastpipe = state.shell_opts.lastpipe && !job_control_active(state);

    let mut prev_read_fd: Option<RawFd> = None;
    let mut child_pids: Vec<Pid> = Vec::new();
    let mut pgid = Pid::from_raw(0);
    let mut last_in_shell: Option<i32> = None;
    // What each stage's stdin carries; shadowing builtins such as `sort` only
    // take typed values from an upstream value stage (see shadowing.rs).
    let mut stage_origin = crate::shadowing::stdin_origin();

    for (i, cmd) in cmds.iter().enumerate() {
        let is_last = i == cmds.len() - 1;

        let (read_fd, write_fd): (Option<RawFd>, Option<RawFd>) = if !is_last {
            match pipe() {
                Ok((r, w)) => (Some(r.into_raw_fd()), Some(w.into_raw_fd())),
                Err(e) => {
                    eprintln!("jsh: pipe failed: {}", e);
                    if let Some(fd) = prev_read_fd {
                        close(fd).ok();
                    }
                    return 1;
                }
            }
        } else {
            (None, None)
        };

        if use_lastpipe && is_last {
            // Parent keeps prev_read_fd as the last stage's stdin.
            let saved_stdin = prev_read_fd
                .and_then(|_| unsafe { nix::unistd::dup(BorrowedFd::borrow_raw(0)) }.ok());
            if let Some(fd) = prev_read_fd.take() {
                dup2_raw(fd, 0).ok();
                close(fd).ok();
            }
            let origin = crate::shadowing::stage_input_origin(cmd, stage_origin, state);
            let saved_origin = crate::shadowing::set_stdin_origin(origin);
            let code = execute_command(cmd, state);
            crate::shadowing::set_stdin_origin(saved_origin);
            if let Some(sfd) = saved_stdin {
                dup2_raw(sfd.as_raw_fd(), 0).ok();
            }
            last_in_shell = Some(code);
            break;
        }

        state
            .fork_count
            .fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        match unsafe { fork() } {
            Ok(ForkResult::Child) => {
                signal::reset_child_signals();
                state.interactive = false;
                if !use_lastpipe {
                    let my_pid = nix::unistd::getpid();
                    let target_pgid = if pgid.as_raw() == 0 { my_pid } else { pgid };
                    setpgid(my_pid, target_pgid).ok();
                }

                if let Some(fd) = prev_read_fd {
                    dup2_raw(fd, 0).ok();
                    close(fd).ok();
                }
                if let Some(fd) = write_fd {
                    dup2_raw(fd, 1).ok();
                    close(fd).ok();
                }
                if let Some(fd) = read_fd {
                    close(fd).ok();
                }

                let origin = crate::shadowing::stage_input_origin(cmd, stage_origin, state);
                crate::shadowing::set_stdin_origin(origin);
                let code = execute_command_in_pipeline_child(cmd, state);
                child_exit(code);
            }
            Ok(ForkResult::Parent { child }) => {
                if !use_lastpipe {
                    let first_child = pgid.as_raw() == 0;
                    if first_child {
                        pgid = child;
                    }
                    setpgid(child, pgid).ok();
                    if first_child {
                        signal::set_foreground_pgid(Some(pgid.as_raw()));
                    }
                }
                child_pids.push(child);
                if let Some(fd) = write_fd {
                    close(fd).ok();
                }
                if let Some(fd) = prev_read_fd {
                    close(fd).ok();
                }
                prev_read_fd = read_fd;
                stage_origin = crate::shadowing::stage_output_origin(cmd, stage_origin, state);
            }
            Err(e) => {
                if !use_lastpipe {
                    signal::set_foreground_pgid(None);
                }
                eprintln!("jsh: fork failed: {}", e);
                return 1;
            }
        }
    }

    let shell_pgid = nix::unistd::getpgrp();
    let give_terminal = !use_lastpipe && state.interactive && pgid.as_raw() != 0;
    if give_terminal {
        tcsetpgrp(std::io::stdin(), pgid).ok();
    }

    let mut last_status = 0;
    let mut pipestatus = Vec::new();
    for pid in child_pids {
        match waitpid(pid, None) {
            Ok(WaitStatus::Exited(_, code)) => {
                pipestatus.push(code);
                last_status = code;
            }
            Ok(WaitStatus::Signaled(_, sig, _)) => {
                let c = 128 + sig as i32;
                pipestatus.push(c);
                last_status = c;
            }
            _ => {
                pipestatus.push(1);
                last_status = 1;
            }
        }
    }
    if let Some(code) = last_in_shell {
        pipestatus.push(code);
        last_status = code;
    }
    if !use_lastpipe {
        signal::set_foreground_pgid(None);
    }

    if give_terminal {
        tcsetpgrp(std::io::stdin(), shell_pgid).ok();
    }
    if state.shell_opts.pipefail {
        if let Some(&code) = pipestatus.iter().rev().find(|&&c| c != 0) {
            last_status = code;
        }
    }
    state.pipestatus = pipestatus.clone();
    state.set_array(
        "PIPESTATUS",
        pipestatus.iter().map(|c| c.to_string()).collect(),
    );
    // A pipeline reports its aggregate status as a new command boundary.
    state.last_failure_exempt = false;

    if pipeline.negated {
        if last_status == 0 {
            1
        } else {
            0
        }
    } else {
        last_status
    }
}

fn execute_command(cmd: &Command, state: &mut ShellState) -> i32 {
    match cmd {
        Command::Simple(simple) => {
            let code = execute_simple(simple, state);
            // Function/eval/source return statuses are new command statuses;
            // they do not inherit the body's final AND/OR exemption.
            state.last_failure_exempt = false;
            code
        }
        Command::Compound(compound) => execute_compound(compound, state),
        Command::FunctionDef { name, body } => {
            state.functions.insert(name.clone(), *body.clone());
            0
        }
    }
}

fn execute_command_in_pipeline_child(cmd: &Command, state: &mut ShellState) -> i32 {
    match cmd {
        Command::Simple(simple) => execute_simple_with_mode(simple, state, false),
        _ => execute_command(cmd, state),
    }
}

/// Perform one assignment and return how xtrace should render it. The rendering
/// is built from the value that was actually assigned, because expanding the
/// word a second time just to trace it would run its command substitutions
/// twice.
fn execute_assignment(assign: &Assignment, state: &mut ShellState) -> String {
    if let Some(ref array_words) = assign.array_value {
        // Array assignment: arr=(a b c). Each element is a normal word, so an
        // unquoted expansion inside one can split into several elements and
        // globs expand against the filesystem.
        let mut values: Vec<String> = Vec::new();
        for w in array_words {
            values.extend(expand_word(w, state));
        }
        let trace = format!(
            "{}=({})",
            assign.name,
            values
                .iter()
                .map(|v| xtrace_quote(v))
                .collect::<Vec<_>>()
                .join(" ")
        );
        if assign.append {
            let arr = state.arrays.entry(assign.name.clone()).or_default();
            arr.extend(values);
        } else {
            state.arrays.insert(assign.name.clone(), values);
        }
        trace
    } else if let Some(ref index) = assign.index {
        // Indexed assignment: arr[idx]=value. Bash expands the value first,
        // then resolves the subscript (an arithmetic index unless the array
        // is associative): `i=0; a[i+1]=$((i=7))` stores 7 at index 8.
        let value = expand_word_to_string(&assign.value, state);
        if let Some(key) = crate::expand::resolve_array_subscript(&assign.name, index, state) {
            state.set_array_element(&assign.name, &key, &value);
        }
        format!("{}[{}]={}", assign.name, index, xtrace_quote(&value))
    } else if assign.append {
        // String append: var+=value
        let value = expand_word_to_string(&assign.value, state);
        if state.is_array(&assign.name) {
            let arr = state.arrays.entry(assign.name.clone()).or_default();
            arr.push(value.clone());
        } else {
            let old = state.get_var(&assign.name).unwrap_or("").to_string();
            state.set_var(&assign.name, &format!("{}{}", old, value));
        }
        format!("{}+={}", assign.name, xtrace_quote(&value))
    } else {
        let value = expand_word_to_string(&assign.value, state);
        state.set_var(&assign.name, &value);
        format!("{}={}", assign.name, xtrace_quote(&value))
    }
}

/// `let NAME = ...` form: `=` must be the entire third word (a bare literal).
/// Bash arithmetic `let "x = 1+2"` quotes the assignment into one word.
fn is_typed_let(words: &[Word]) -> bool {
    if words.len() < 4 {
        return false;
    }
    let head_is_let = matches!(words[0].as_slice(), [WordPart::Literal(s)] if s == "let");
    let eq_is_bare = matches!(words[2].as_slice(), [WordPart::Literal(s)] if s == "=");
    head_is_let && eq_is_bare && is_simple_ident(&words[1])
}

fn is_simple_ident(w: &Word) -> bool {
    match w.as_slice() {
        [WordPart::Literal(s)] => {
            !s.is_empty()
                && s.chars()
                    .next()
                    .map(|c| c.is_alphabetic() || c == '_')
                    .unwrap_or(false)
                && s.chars().all(|c| c.is_alphanumeric() || c == '_')
        }
        _ => false,
    }
}

fn execute_typed_let(words: &[Word], state: &mut ShellState) -> i32 {
    use crate::value::{ClosureData, Value};
    use std::sync::Arc;

    let name = match &words[1][0] {
        WordPart::Literal(s) => s.clone(),
        _ => return 1,
    };
    let rhs = &words[3..];
    let value = build_let_value(rhs, state);
    state.let_vars.insert(name, value);
    return 0;

    // unused suppressed; type imports above
    #[allow(dead_code)]
    fn _refs(_: Value, _: Arc<ClosureData>) {}
}

fn build_let_value(rhs: &[Word], state: &mut ShellState) -> crate::value::Value {
    use crate::value::{ClosureData, Value};
    use std::sync::Arc;

    // Single closure literal — bind with captured snapshot.
    if rhs.len() == 1 && rhs[0].len() == 1 {
        if let WordPart::Closure { params, body_src } = &rhs[0][0] {
            return Value::Closure(Arc::new(ClosureData {
                params: params.clone(),
                body_src: body_src.clone(),
                captured: state.let_vars.clone(),
            }));
        }
    }
    // Otherwise expand & sniff. Pure-literal RHS (no expansion) goes through
    // JSON; expanded text falls back to JSON if it parses, else String.
    let joined: String = rhs
        .iter()
        .map(|w| expand_word_to_string(w, state))
        .collect::<Vec<_>>()
        .join(" ");
    let trimmed = joined.trim();
    if trimmed.is_empty() {
        return Value::Null;
    }
    if let Ok(j) = serde_json::from_str::<serde_json::Value>(trimmed) {
        return Value::from_json(j);
    }
    Value::String(trimmed.to_string())
}

/// Apply a closure to a positional argument list, returning the body's value.
/// Establishes a fresh let_vars scope = captured ∪ params, runs the body via
/// the parser/executor, and returns the closure's "result Value".
///
/// Result extraction: closures whose body is a single value-aware pipeline
/// already produce a `PipelineData::Values(...)` we can collect. For the
/// general case (single expression like `$x.age -gt 30`), we re-parse the body
/// and run it through `execute_value_pipeline`-compatible path; if that's not
/// available, we capture stdout as a string Value.
pub fn apply_closure(
    closure: &crate::value::ClosureData,
    args: &[crate::value::Value],
    state: &mut ShellState,
) -> Result<crate::value::Value, i32> {
    use crate::pipeline_data::PipelineData;
    use crate::value::Value;
    use std::collections::HashMap;

    // Literal values and pure expressions never need to install a local shell
    // scope. Their parsed expression and borrowed bindings are reusable across
    // every row without cloning the captured map or entire argument records.
    // Expression-body shortcut: `{|a, b| $a + $b}`, `{|r| if $r.x > 5 ... }`,
    // etc. Runs BEFORE installing into state so the bulk-iteration case
    // (each / where / reduce) avoids per-iteration state churn.
    match crate::closure_expr::try_eval_with(&closure.body_src, |name| closure.lookup(args, name)) {
        Ok(Some(v)) => return Ok(v),
        Ok(None) => {} // not a pure expression — fall through to shell path
        Err(msg) => {
            state.set_error(msg, 1);
            return Err(1);
        }
    }

    // Fall through to the shell-command path: now we need state.let_vars to
    // hold the local view so variable resolution inside the parsed body works.
    let mut local_vars: HashMap<String, Value> = closure.captured.clone();
    for (index, parameter) in closure.params.iter().enumerate() {
        local_vars.insert(
            parameter.clone(),
            args.get(index).cloned().unwrap_or(Value::Null),
        );
    }
    local_vars
        .entry("in".into())
        .or_insert_with(|| args.first().cloned().unwrap_or(Value::Null));
    let saved = std::mem::replace(&mut state.let_vars, local_vars);

    let result = (|| -> Result<Value, i32> {
        let parsed = crate::parser::parse(&closure.body_src).map_err(|_| 2_i32)?;
        // Pure-variable-path body shortcut: `{|r| $r.a}` → return the typed
        // value of that path, not "run $r.a as a command".
        if parsed.len() == 1 {
            let pipeline = &parsed[0].list.first;
            if pipeline.commands.len() == 1 {
                if let crate::parser::ast::Command::Simple(s) = &pipeline.commands[0] {
                    if s.words.len() == 1 && s.assignments.is_empty() && s.redirects.is_empty() {
                        if let [crate::parser::ast::WordPart::VariablePath { name, path }] =
                            s.words[0].as_slice()
                        {
                            if let Some(base) = state.let_vars.get(name).cloned() {
                                return Ok(crate::expand::resolve_path(&base, path)
                                    .cloned()
                                    .unwrap_or(Value::Null));
                            }
                        }
                    }
                }
            }
        }
        let mut last: Value = Value::Null;
        for complete in &parsed {
            // Try the value-aware pipeline path so the closure returns a Value
            // instead of writing to stdout. If the body's first pipeline isn't
            // all-value-aware, fall back to running it for its exit status and
            // returning Bool(exit==0).
            let pipeline = &complete.list.first;
            if !pipeline.commands.is_empty() && pipeline.commands.iter().all(is_value_aware_command)
            {
                // The body's pipeline starts with the first argument as input,
                // so `each {|x| select name}` projects from `x`.
                let mut data = match args.first() {
                    Some(v) => PipelineData::Values(vec![v.clone()]),
                    None => PipelineData::Empty,
                };
                for cmd in &pipeline.commands {
                    let simple = match cmd {
                        crate::parser::ast::Command::Simple(s) => s,
                        _ => unreachable!(),
                    };
                    let expanded = expand_words(&simple.words, state);
                    if let Some(code) = report_expansion_error(state) {
                        return Err(code);
                    }
                    if expanded.is_empty() {
                        continue;
                    }
                    let name = &expanded[0];
                    let extra_args = &expanded[1..];
                    let f = crate::value_builtins::VALUE_BUILTINS
                        .get(name.as_str())
                        .ok_or(2_i32)?;
                    if let Some(sig) = crate::signature::SIGNATURES.get(name.as_str()) {
                        if let Err(msg) = sig.validate_args(extra_args) {
                            state.set_error(msg.clone(), 2);
                            eprintln!("jsh: {}", msg);
                            return Err(2);
                        }
                    }
                    data = match f(data, extra_args, state) {
                        Ok(data) => data,
                        Err(code) => {
                            // Surface a lazy stage's failure as this closure's
                            // error, which `try` can catch, instead of leaving
                            // it pending for an unrelated pipeline to report.
                            if let Some(message) = crate::pipeline_data::take_stream_failure() {
                                state.set_error(message, code);
                            }
                            return Err(code);
                        }
                    };
                }
                let data = match data {
                    PipelineData::Stream(it) => PipelineData::Values(it.collect()),
                    other => other,
                };
                if let Some(message) = crate::pipeline_data::take_stream_failure() {
                    state.set_error(message, 1);
                    return Err(1);
                }
                last = match data {
                    PipelineData::Values(mut vs) if vs.len() == 1 => vs.remove(0),
                    PipelineData::Values(vs) => Value::List(vs),
                    PipelineData::Bytes(b) => {
                        Value::String(String::from_utf8_lossy(&b).to_string())
                    }
                    PipelineData::Empty => Value::Null,
                    PipelineData::Stream(_) => unreachable!("normalized above"),
                };
            } else {
                // Fall back: run as a normal command (e.g. `test ...` style).
                let code = execute_complete_command(complete, state);
                last = Value::Bool(code == 0);
            }
        }
        Ok(last)
    })();

    state.let_vars = saved;
    result
}

fn execute_simple(cmd: &SimpleCommand, state: &mut ShellState) -> i32 {
    execute_simple_with_mode(cmd, state, true)
}

thread_local! {
    /// Alias names currently being expanded. Bash stops re-expanding a word that
    /// is already in the chain, which is what keeps `alias ls='ls -la'` (and the
    /// mutual `alias a=b; alias b=a`) from looping forever.
    static ALIAS_CHAIN: std::cell::RefCell<Vec<String>> = const { std::cell::RefCell::new(Vec::new()) };
}

struct AliasGuard;

impl AliasGuard {
    fn enter(name: &str) -> Option<AliasGuard> {
        ALIAS_CHAIN.with(|chain| {
            let mut chain = chain.borrow_mut();
            if chain.iter().any(|n| n == name) {
                return None;
            }
            chain.push(name.to_string());
            Some(AliasGuard)
        })
    }
}

impl Drop for AliasGuard {
    fn drop(&mut self) {
        ALIAS_CHAIN.with(|chain| {
            chain.borrow_mut().pop();
        });
    }
}

/// The alias name a command word could stand for.
///
/// Only a bare, unquoted literal can name an alias: bash substitutes aliases
/// textually before any expansion, so `$cmd` or `"ls"` never triggers one.
fn alias_head(cmd: &SimpleCommand) -> Option<&str> {
    match cmd.words.first()?.as_slice() {
        [WordPart::Literal(s)] => Some(s.as_str()),
        _ => None,
    }
}

fn parse_cached(source: &str) -> Option<Vec<CompleteCommand>> {
    if let Some(cached) = crate::parser::cache::cache_get(source) {
        return Some(cached);
    }
    let parsed = crate::parser::parse(source).ok()?;
    crate::parser::cache::cache_insert(source.to_string(), parsed.clone());
    Some(parsed)
}

/// The first / last `Simple` command of a parsed alias body, which is where the
/// caller's assignment prefix and trailing arguments belong (`alias ll='ls|less'`
/// plus `ll /tmp` is `ls | less /tmp` in bash).
fn simple_at(program: &mut [CompleteCommand], last: bool) -> Option<&mut SimpleCommand> {
    let cmd = if last {
        program.last_mut()?
    } else {
        program.first_mut()?
    };
    let pipeline = if last {
        match cmd.list.rest.last_mut() {
            Some((_, p)) => p,
            None => &mut cmd.list.first,
        }
    } else {
        &mut cmd.list.first
    };
    let command = if last {
        pipeline.commands.last_mut()?
    } else {
        pipeline.commands.first_mut()?
    };
    match command {
        Command::Simple(s) => Some(s),
        _ => None,
    }
}

/// Alias-expand the word at `idx` in place.
///
/// Bash re-checks the next word for an alias when the alias body ends in a
/// space, which is what makes `alias sudo='sudo '` expand the command after it.
fn expand_trailing_alias(words: &mut Vec<Word>, mut idx: usize, state: &ShellState) {
    let mut steps = 0;
    while idx < words.len() && steps < 16 {
        steps += 1;
        let name = match words[idx].as_slice() {
            [WordPart::Literal(s)] => s.clone(),
            _ => return,
        };
        let body = match state.aliases.get(&name) {
            Some(b) => b.clone(),
            None => return,
        };
        let _guard = match AliasGuard::enter(&name) {
            Some(g) => g,
            None => return,
        };
        // Only a plain word list can be spliced in mid-command; a body with a
        // pipeline or a redirection is not a valid argument position.
        let mut program = match parse_cached(&body) {
            Some(p) => p,
            None => return,
        };
        if program.len() != 1 {
            return;
        }
        let replacement = match simple_at(&mut program, false) {
            Some(s)
                if s.redirects.is_empty() && s.assignments.is_empty() && !s.words.is_empty() =>
            {
                s.words.clone()
            }
            _ => return,
        };
        let len = replacement.len();
        words.splice(idx..idx + 1, replacement);
        if !body.ends_with(' ') && !body.ends_with('\t') {
            return;
        }
        // This body also ends in a space, so the word after the substitution is
        // itself a candidate.
        idx += len;
    }
}

/// Substitute an alias for the command word and run the result.
///
/// The alias body is TEXTUAL: it is parsed, and the caller's remaining words are
/// carried over as already-parsed words. Re-joining the expanded argv into a
/// string and re-parsing it (what jsh used to do) ran a second expansion pass,
/// so `e '$(echo INJECTED)'` executed the substitution that single quotes were
/// supposed to protect, and the early return skipped redirection setup entirely.
fn try_alias(cmd: &SimpleCommand, state: &mut ShellState) -> Option<i32> {
    let name = alias_head(cmd)?.to_string();
    let body = state.aliases.get(&name)?.clone();
    let guard = AliasGuard::enter(&name)?;
    let mut program = parse_cached(&body)?;
    if program.is_empty() {
        return None;
    }

    // The caller's trailing arguments join the last command of the body; its
    // redirections come after the body's own, so `alias e='echo >a'; e >b`
    // writes to b, exactly as the textual `echo >a >b` would.
    let mut rest: Vec<Word> = cmd.words[1..].to_vec();
    if !rest.is_empty() && (body.ends_with(' ') || body.ends_with('\t')) {
        expand_trailing_alias(&mut rest, 0, state);
    }
    match simple_at(&mut program, true) {
        Some(tail) => {
            tail.words.extend(rest);
            tail.redirects.extend(cmd.redirects.iter().cloned());
        }
        None if rest.is_empty() && cmd.redirects.is_empty() => {}
        // A compound body (`alias x='if ...; fi'`) has nowhere to put the
        // caller's extras; bash would produce a syntax error, so decline the
        // alias rather than silently dropping them.
        None => return None,
    }
    if let Some(head) = simple_at(&mut program, false) {
        for (i, assign) in cmd.assignments.iter().enumerate() {
            head.assignments.insert(i, assign.clone());
        }
    }

    let mut last = 0;
    for c in &program {
        last = execute_complete_command(c, state);
        if control_flow_requested(state) {
            break;
        }
    }
    drop(guard);
    Some(last)
}

fn execute_simple_with_mode(
    cmd: &SimpleCommand,
    state: &mut ShellState,
    fork_external: bool,
) -> i32 {
    // Aliases are substituted for the command word before anything is expanded,
    // like bash does at parse time.
    if let Some(code) = try_alias(cmd, state) {
        return code;
    }

    // `set -u` has to look at the words BEFORE expansion: expansion turns an
    // unset parameter into the empty string and the distinction is gone.
    if state.shell_opts.nounset {
        let assignment_values: Vec<Word> = cmd
            .assignments
            .iter()
            .map(|a| a.value.clone())
            .chain(
                cmd.assignments
                    .iter()
                    .filter_map(|a| a.array_value.clone())
                    .flatten(),
            )
            .collect();
        if let Some(param) = nounset_violation(&cmd.words, state)
            .or_else(|| nounset_violation(&assignment_values, state))
        {
            return report_nounset(state, &param);
        }
    }

    // Handle assignments only (no command)
    if cmd.words.is_empty() {
        // Bash gives an assignment-only command the status of the last command
        // substitution it ran: `out=$(false); echo $?` prints 1.
        state.last_command_sub_status = None;
        // Bash applies these redirections even without a command. Assignment
        // values still take effect after an ordinary I/O failure, but fatal
        // expansion errors stop the program before any assignment.
        let redirects = setup_redirects(&cmd.redirects, state);
        if state.abort_current_program {
            return 1;
        }
        let mut traced = Vec::new();
        for assign in &cmd.assignments {
            traced.push(execute_assignment(assign, state));
            if let Some(code) = report_expansion_error(state) {
                return code;
            }
        }
        if state.shell_opts.xtrace {
            eprintln!("{}{}", xtrace_prefix(state), traced.join(" "));
        }
        return match redirects {
            Ok(_guard) => state.last_command_sub_status.take().unwrap_or(0),
            Err(code) => code,
        };
    }

    // Phase 5b: nushell-style `let NAME = EXPR`.
    // Disambiguated from bash arithmetic `let "x = 1+2"` by requiring `=` as a
    // separate, bare word (no quoting). We access raw WordParts here so a
    // closure literal `{|x|...}` reaches build_value_from_words intact.
    if is_typed_let(&cmd.words) {
        return with_redirects(&cmd.redirects, state, |state| {
            execute_typed_let(&cmd.words, state)
        });
    }

    // Expand words. A `[[ ]]` conditional keeps one operand per word: the
    // parser only ever produces a bare `[[` head word for that construct.
    let expanded = if is_conditional_command(&cmd.words) {
        crate::expand::expand_conditional_words(&cmd.words, state)
    } else {
        expand_words(&cmd.words, state)
    };
    if let Some(code) = report_expansion_error(state) {
        return code;
    }
    if expanded.is_empty() {
        return with_redirects(&cmd.redirects, state, |_| 0);
    }

    let cmd_name = &expanded[0];
    let args = &expanded[1..];

    if state.shell_opts.xtrace {
        xtrace_line(state, &expanded);
    }

    // Check for function
    if let Some(func_body) = state.functions.get(cmd_name).cloned() {
        // A function call carries its redirections and its assignment prefix
        // exactly like a builtin does: `f 2>/dev/null` must silence everything
        // the body writes, and `V=x f` must expose V for the call only.
        let _redirects = match setup_redirects(&cmd.redirects, state) {
            Ok(guard) => guard,
            Err(code) => return code,
        };
        let saved_vars: Vec<(String, Option<String>)> = cmd
            .assignments
            .iter()
            .map(|a| {
                let old = state.get_var(&a.name).map(|s| s.to_string());
                let val = expand_word_to_string(&a.value, state);
                state.set_var(&a.name, &val);
                (a.name.clone(), old)
            })
            .collect();

        state.push_local_scope();
        state.push_positional_params(args.to_vec());
        let caller_loop_depth = std::mem::replace(&mut state.loop_depth, 0);
        state.return_depth += 1;
        let code = execute_compound(&func_body, state);
        state.return_depth -= 1;
        state.loop_depth = caller_loop_depth;
        state.pop_positional_params();
        state.pop_local_scope();

        for (name, old) in saved_vars {
            match old {
                Some(v) => state.set_var(&name, &v),
                None => state.unset_var(&name),
            }
        }

        // Handle return statement in function
        let return_code = if state.return_requested {
            let ret = state.return_value;
            state.return_requested = false;
            state.return_value = 0;
            ret
        } else {
            code
        };

        return return_code;
    }

    // Phase 15c — typed user function registered via `def`. Validate arity
    // via RuntimeSignature, coerce each arg to a Value (best effort), run the
    // captured closure, then print the resulting value.
    if state.user_typed_fns.contains_key(cmd_name) {
        let _redirects = match setup_redirects(&cmd.redirects, state) {
            Ok(guard) => guard,
            Err(code) => return code,
        };
        let sig = state.user_signatures.get(cmd_name).cloned();
        if let Some(s) = &sig {
            if let Err(msg) = s.validate_args(args) {
                eprintln!("jsh: {}", msg);
                state.set_error(msg, 2);
                return 2;
            }
        }
        let closure = state.user_typed_fns.get(cmd_name).cloned().unwrap();
        let mut call_args: Vec<crate::value::Value> = Vec::with_capacity(args.len());
        for (i, a) in args.iter().enumerate() {
            // Skip flag tokens for now — only positional args become bound params.
            if a.starts_with('-') && a.len() > 1 && !a.chars().nth(1).unwrap().is_ascii_digit() {
                continue;
            }
            let v = if let Ok(j) = serde_json::from_str::<serde_json::Value>(a) {
                crate::value::Value::from_json(j)
            } else {
                crate::value_builtins::coerce_string_to_value(a)
            };
            if let Some(s) = &sig {
                if let Some(p) = s.params.get(i) {
                    if !p.rest {
                        if let Err(msg) = crate::signature::check_value_type(&v, p.kind, &p.name) {
                            eprintln!("jsh: {}: {}", cmd_name, msg);
                            state.set_error(msg, 2);
                            return 2;
                        }
                    }
                }
            }
            call_args.push(v);
        }
        match crate::executor::apply_closure(&closure, &call_args, state) {
            Ok(v) => {
                let pd = match v {
                    crate::value::Value::Null => crate::pipeline_data::PipelineData::Empty,
                    other => crate::pipeline_data::PipelineData::Values(vec![other]),
                };
                if let Err(e) = pd.write_to_stdout() {
                    eprintln!("jsh: {}", e);
                    return 1;
                }
                return 0;
            }
            Err(code) => return code,
        }
    }

    // Check for builtin. A builtin disabled with `enable -n` resolves through
    // PATH instead, and one that shadows a system program (`sort`, `date`,
    // ...) yields to it unless this invocation is jsh's own form; either way
    // the command continues below as an ordinary external command.
    if builtins::is_enabled_builtin(cmd_name, state)
        && !crate::shadowing::runs_system_program(cmd_name, args, &cmd.redirects)
    {
        let mut redirects = match setup_redirects(&cmd.redirects, state) {
            Ok(guard) => guard,
            Err(code) => return code,
        };
        let saved_vars: Vec<(String, Option<String>, Option<std::ffi::OsString>)> = cmd
            .assignments
            .iter()
            .map(|a| {
                let old = state.get_var(&a.name).map(|s| s.to_string());
                let old_environment = std::env::var_os(&a.name);
                let val = expand_word_to_string(&a.value, state);
                state.set_var(&a.name, &val);
                // Prefix assignments belong to the invoked command's exported
                // environment even when set_var resolves into a function local.
                crate::environment::set_env_checked(&a.name, &val);
                (a.name.clone(), old, old_environment)
            })
            .collect();

        let code = match report_expansion_error(state) {
            Some(code) => code,
            None => builtins::run_builtin(cmd_name, args, state),
        };

        for (name, old, old_environment) in saved_vars.into_iter().rev() {
            match old {
                Some(v) => state.set_var(&name, &v),
                None => state.unset_var(&name),
            }
            match old_environment {
                Some(value) => std::env::set_var(&name, value),
                None => std::env::remove_var(&name),
            }
        }
        if commits_exec_redirects(cmd_name, args) && code == 0 {
            redirects.commit();
        }
        return code;
    }

    // A command that enters a container has its shell replaced first. This
    // sits on the final argv, so it sees exactly what would have run, and it
    // is deliberately not on the `spawn_external` path: `command docker …` is
    // how a person says "run the one I typed".
    let upgraded = crate::container::upgrade_entry(&expanded, state)
        .or_else(|| crate::ssh_entry::upgrade_entry(&expanded, state));
    let expanded: &[String] = upgraded.as_deref().unwrap_or(&expanded);
    let cmd_name = &expanded[0];

    // External command - fork and exec
    if !fork_external {
        if let Err(code) = apply_redirects_in_child(&cmd.redirects, state) {
            return code;
        }

        for assign in &cmd.assignments {
            let val = expand_word_to_string(&assign.value, state);
            std::env::set_var(&assign.name, &val);
        }

        let c_cmd = CString::new(cmd_name.as_str()).unwrap_or_default();
        let c_args: Vec<CString> = expanded
            .iter()
            .map(|s| CString::new(s.as_str()).unwrap_or_default())
            .collect();

        let _ = execvp(&c_cmd, &c_args);
        let (msg, code) = exec_error_info(cmd_name);
        shell_error(&format!("{}: {}", cmd_name, msg));
        if code == 127 {
            if let Some(suggestion) = suggest_command(cmd_name, state) {
                shell_command_hint(&suggestion);
            }
        }
        return code;
    }

    match unsafe { fork() } {
        Ok(ForkResult::Child) => {
            signal::reset_child_signals();
            let pid = nix::unistd::getpid();
            setpgid(pid, pid).ok();

            if let Err(code) = apply_redirects_in_child(&cmd.redirects, state) {
                child_exit(code);
            }

            for assign in &cmd.assignments {
                let val = expand_word_to_string(&assign.value, state);
                std::env::set_var(&assign.name, &val);
            }

            let c_cmd = CString::new(cmd_name.as_str()).unwrap_or_default();
            let c_args: Vec<CString> = expanded
                .iter()
                .map(|s| CString::new(s.as_str()).unwrap_or_default())
                .collect();

            let _ = execvp(&c_cmd, &c_args);
            let (msg, code) = exec_error_info(cmd_name);
            shell_error(&format!("{}: {}", cmd_name, msg));
            child_exit(code);
        }
        Ok(ForkResult::Parent { child }) => {
            setpgid(child, child).ok();
            signal::set_foreground_pgid(Some(child.as_raw()));
            let exit_code = if state.interactive {
                wait_for_fg(child, state)
            } else {
                match waitpid(child, None) {
                    Ok(WaitStatus::Exited(_, code)) => code,
                    Ok(WaitStatus::Signaled(_, sig, _)) => 128 + sig as i32,
                    _ => 1,
                }
            };
            signal::set_foreground_pgid(None);
            if exit_code == 127 {
                if let Some(suggestion) = suggest_command(cmd_name, state) {
                    shell_command_hint(&suggestion);
                }
            }
            exit_code
        }
        Err(e) => {
            shell_error(&format!("fork failed: {}", e));
            1
        }
    }
}

/// `command` preserves exec's special redirection behavior, whereas the
/// ordinary `builtin` wrapper has its own temporary redirection scope.
fn commits_exec_redirects<'a>(mut name: &'a str, mut args: &'a [String]) -> bool {
    loop {
        if name == "exec" {
            return true;
        }
        if name != "command" {
            return false;
        }
        while let Some(option) = args.first() {
            match option.as_str() {
                "-v" | "-V" => return false,
                "-p" => args = &args[1..],
                "--" => {
                    args = &args[1..];
                    break;
                }
                _ => break,
            }
        }
        let Some((next, rest)) = args.split_first() else {
            return false;
        };
        name = next;
        args = rest;
    }
}

/// Run an external program from an already-expanded argv, bypassing functions
/// and aliases. Redirections and assignments belong to the caller's frame and
/// are expected to be in place before this is called.
///
/// The `command` builtin needs this: re-joining argv with spaces and re-parsing
/// the result destroyed every argument that contained whitespace, quotes or a
/// newline (`command sed -e "$multiline_script"` lost its script entirely).
pub fn spawn_external(argv: &[String], state: &mut ShellState) -> i32 {
    let Some(cmd_name) = argv.first() else {
        return 0;
    };
    match unsafe { fork() } {
        Ok(ForkResult::Child) => {
            signal::reset_child_signals();
            let pid = nix::unistd::getpid();
            setpgid(pid, pid).ok();

            let c_cmd = CString::new(cmd_name.as_str()).unwrap_or_default();
            let c_args: Vec<CString> = argv
                .iter()
                .map(|s| CString::new(s.as_str()).unwrap_or_default())
                .collect();

            let _ = execvp(&c_cmd, &c_args);
            let (msg, code) = exec_error_info(cmd_name);
            shell_error(&format!("{}: {}", cmd_name, msg));
            child_exit(code);
        }
        Ok(ForkResult::Parent { child }) => {
            setpgid(child, child).ok();
            signal::set_foreground_pgid(Some(child.as_raw()));
            let exit_code = if state.interactive {
                wait_for_fg(child, state)
            } else {
                match waitpid(child, None) {
                    Ok(WaitStatus::Exited(_, code)) => code,
                    Ok(WaitStatus::Signaled(_, sig, _)) => 128 + sig as i32,
                    _ => 1,
                }
            };
            signal::set_foreground_pgid(None);
            exit_code
        }
        Err(e) => {
            shell_error(&format!("fork failed: {}", e));
            1
        }
    }
}

// Helper function to wrap compound command execution with redirect handling
fn with_redirects<F>(redirects: &[Redirect], state: &mut ShellState, f: F) -> i32
where
    F: FnOnce(&mut ShellState) -> i32,
{
    let _redirects = match setup_redirects(redirects, state) {
        Ok(guard) => guard,
        Err(code) => return code,
    };
    f(state)
}

pub fn execute_compound(cmd: &CompoundCommand, state: &mut ShellState) -> i32 {
    state.last_failure_exempt = false;
    match cmd {
        CompoundCommand::BraceGroup { body, redirects } => {
            with_redirects(redirects, state, |state| execute_command_list(body, state))
        }
        CompoundCommand::Subshell { body, redirects } => match unsafe { fork() } {
            Ok(ForkResult::Child) => {
                signal::reset_child_signals();
                let pid = nix::unistd::getpid();
                setpgid(pid, pid).ok();
                state.loop_depth = 0;
                if let Err(code) = apply_redirects_in_child(redirects, state) {
                    child_exit(code);
                }
                let code = execute_command_list(body, state);
                child_exit(code);
            }
            Ok(ForkResult::Parent { child }) => {
                setpgid(child, child).ok();
                signal::set_foreground_pgid(Some(child.as_raw()));
                let code = if state.interactive {
                    wait_for_fg(child, state)
                } else {
                    match waitpid(child, None) {
                        Ok(WaitStatus::Exited(_, code)) => code,
                        Ok(WaitStatus::Signaled(_, sig, _)) => 128 + sig as i32,
                        _ => 1,
                    }
                };
                signal::set_foreground_pgid(None);
                code
            }
            Err(_) => 1,
        },
        CompoundCommand::If {
            conditions,
            else_branch,
            redirects,
        } => with_redirects(redirects, state, |state| {
            let mut code = 0;
            let mut matched = false;

            for (condition, body) in conditions {
                let cond_code = execute_condition(condition, state);
                if control_flow_requested(state) {
                    return cond_code;
                }
                if cond_code == 0 {
                    code = execute_command_list(body, state);
                    matched = true;
                    break;
                }
            }

            if !matched {
                if let Some(else_body) = else_branch {
                    code = execute_command_list(else_body, state);
                } else {
                    state.last_failure_exempt = false;
                }
            }
            code
        }),
        CompoundCommand::For {
            var,
            words,
            body,
            redirects,
        } => {
            with_redirects(redirects, state, |state| {
                let word_list = match words {
                    Some(ws) => expand_words(ws, state),
                    None => state.positional_params.clone(),
                };
                if let Some(code) = report_expansion_error(state) {
                    return code;
                }

                let mut code = 0;
                state.loop_depth += 1;
                for w in &word_list {
                    state.set_var(var, w);
                    code = execute_command_list(body, state);

                    if execution_unwind_requested(state) {
                        break;
                    }

                    // Check for break/continue control flow
                    if state.loop_break {
                        state.loop_break = false;
                        break;
                    }
                    if state.loop_continue {
                        state.loop_continue = false;
                        continue;
                    }
                }
                state.loop_depth -= 1;
                code
            })
        }
        CompoundCommand::CStyleFor {
            init,
            condition,
            update,
            body,
            redirects,
        } => {
            with_redirects(redirects, state, |state| {
                // A header clause that fails to evaluate ends the loop with
                // status 1, but bash does not apply errexit to that failure:
                // `set -e; for ((i=0; i<1/0; i++)) ...; echo after` reaches
                // `echo`. Hence the exemption on each failure below.
                if !init.is_empty() {
                    if let Err(code) = run_arith_clause(init, state) {
                        state.last_failure_exempt = true;
                        return code;
                    }
                }

                let mut code = 0;
                let mut failure_exempt = false;
                state.loop_depth += 1;
                loop {
                    // Check condition
                    let cond_result = if condition.is_empty() {
                        // Empty condition means infinite loop (like for ((;;)))
                        true
                    } else {
                        match run_arith_clause(condition, state) {
                            Ok(value) => value != 0,
                            Err(status) => {
                                code = status;
                                failure_exempt = true;
                                break;
                            }
                        }
                    };

                    if !cond_result {
                        break;
                    }

                    // Execute body
                    code = execute_command_list(body, state);
                    failure_exempt = state.last_failure_exempt;

                    if execution_unwind_requested(state) {
                        break;
                    }

                    // Check for break/continue
                    if state.loop_break {
                        state.loop_break = false;
                        break;
                    }
                    if state.loop_continue {
                        state.loop_continue = false;
                        // Continue to update without breaking
                    }

                    // Execute update expression
                    if !update.is_empty() {
                        if let Err(status) = run_arith_clause(update, state) {
                            code = status;
                            failure_exempt = true;
                            break;
                        }
                    }
                }
                state.loop_depth -= 1;
                state.last_failure_exempt = failure_exempt;
                code
            })
        }
        CompoundCommand::While {
            condition,
            body,
            redirects,
        } => with_redirects(redirects, state, |state| {
            let mut code = 0;
            let mut failure_exempt = false;
            state.loop_depth += 1;
            loop {
                let cond = execute_condition(condition, state);
                if execution_unwind_requested(state) {
                    code = cond;
                    break;
                }
                if state.loop_break {
                    state.loop_break = false;
                    code = cond;
                    failure_exempt = false;
                    break;
                }
                if cond != 0 {
                    state.loop_continue = false;
                    break;
                }
                if state.loop_continue {
                    state.loop_continue = false;
                    continue;
                }
                code = execute_command_list(body, state);
                failure_exempt = state.last_failure_exempt;
                if execution_unwind_requested(state) {
                    break;
                }
                if state.loop_break {
                    state.loop_break = false;
                    break;
                }
                if state.loop_continue {
                    state.loop_continue = false;
                }
            }
            state.loop_depth -= 1;
            state.last_failure_exempt = failure_exempt;
            code
        }),
        CompoundCommand::Until {
            condition,
            body,
            redirects,
        } => with_redirects(redirects, state, |state| {
            let mut code = 0;
            let mut failure_exempt = false;
            state.loop_depth += 1;
            loop {
                let cond = execute_condition(condition, state);
                if execution_unwind_requested(state) {
                    code = cond;
                    break;
                }
                if state.loop_break {
                    state.loop_break = false;
                    code = cond;
                    failure_exempt = false;
                    break;
                }
                if cond == 0 {
                    state.loop_continue = false;
                    break;
                }
                if state.loop_continue {
                    state.loop_continue = false;
                    continue;
                }
                code = execute_command_list(body, state);
                failure_exempt = state.last_failure_exempt;
                if execution_unwind_requested(state) {
                    break;
                }
                if state.loop_break {
                    state.loop_break = false;
                    break;
                }
                if state.loop_continue {
                    state.loop_continue = false;
                }
            }
            state.loop_depth -= 1;
            state.last_failure_exempt = failure_exempt;
            code
        }),
        CompoundCommand::Case {
            word,
            arms,
            redirects,
        } => {
            with_redirects(redirects, state, |state| {
                let value = expand_word_to_string(word, state);
                let mut last = 0;
                let mut i = 0;
                // `fall` is true when the previous arm ended with ;& and we must
                // run this arm's body unconditionally.
                let mut fall = false;
                while i < arms.len() {
                    let arm = &arms[i];
                    let hit = fall
                        || arm.patterns.iter().any(|p| {
                            let pat = expand_word_to_string(p, state);
                            match_pattern(&value, &pat, state.shell_opts.extglob)
                        });
                    if hit {
                        last = execute_command_list(&arm.body, state);
                        if control_flow_requested(state) {
                            return last;
                        }
                        match arm.terminator {
                            CaseTerminator::Break => return last,
                            CaseTerminator::FallThrough => {
                                fall = true;
                                i += 1;
                            }
                            CaseTerminator::ContinueMatch => {
                                fall = false;
                                i += 1;
                            }
                        }
                    } else {
                        i += 1;
                    }
                }
                last
            })
        }
        CompoundCommand::Select {
            var,
            words,
            body,
            redirects,
        } => {
            with_redirects(redirects, state, |state| {
                // Expand items list
                let items = match words {
                    Some(ws) => expand_words(ws, state),
                    None => state.positional_params.clone(),
                };
                if let Some(code) = report_expansion_error(state) {
                    return code;
                }

                if items.is_empty() {
                    return 0;
                }

                state.loop_depth += 1;
                let code = loop {
                    // Display menu
                    for (i, item) in items.iter().enumerate() {
                        println!("{}) {}", i + 1, item);
                    }

                    // Get PS3 prompt (default "#? ")
                    let ps3 = state.get_var("PS3").unwrap_or("#? ").to_string();
                    eprint!("{}", ps3);
                    use std::io::Write;
                    std::io::stderr().flush().ok();

                    // Read user input
                    let mut reply = String::new();
                    match std::io::stdin().read_line(&mut reply) {
                        Ok(0) => {
                            // EOF reached
                            break 0;
                        }
                        Ok(_) => {
                            let reply_trimmed = reply.trim_end_matches('\n').trim_end_matches('\r');
                            state.set_var("REPLY", reply_trimmed);

                            // Validate selection
                            if let Ok(n) = reply_trimmed.parse::<usize>() {
                                if n >= 1 && n <= items.len() {
                                    let selected = &items[n - 1];
                                    state.set_var(var, selected);
                                    let code = execute_command_list(body, state);

                                    if execution_unwind_requested(state) {
                                        break code;
                                    }

                                    // Check for break/continue control flow
                                    if state.loop_break {
                                        state.loop_break = false;
                                        break code;
                                    }
                                    if state.loop_continue {
                                        state.loop_continue = false;
                                        continue;
                                    }
                                    continue;
                                }
                                // Invalid choice (out of range): show menu again without executing body
                            }
                            // Empty input or non-numeric: show menu again
                        }
                        Err(_) => {
                            break 1;
                        }
                    }
                };
                state.loop_depth -= 1;

                code
            })
        }
        CompoundCommand::Arithmetic { expr, redirects } => {
            with_redirects(redirects, state, |state| {
                // In bash, (( expr )) returns 0 if expr != 0, and 1 if expr == 0
                // This is the opposite of C - expressions are checked for truthiness
                match run_arith_clause(expr, state) {
                    Ok(value) => i32::from(value == 0),
                    Err(status) => status,
                }
            })
        }
        CompoundCommand::Coproc {
            name,
            command,
            redirects,
        } => {
            with_redirects(redirects, state, |state| {
                // Create two pipes for bidirectional communication
                // Pipe 1: parent writes to child's stdin
                // Pipe 2: parent reads from child's stdout
                let (read_from_parent, write_to_child) = match pipe() {
                    Ok(p) => p,
                    Err(e) => {
                        eprintln!("jsh: pipe failed: {}", e);
                        return 1;
                    }
                };
                let (read_from_child, write_to_parent) = match pipe() {
                    Ok(p) => p,
                    Err(e) => {
                        eprintln!("jsh: pipe failed: {}", e);
                        return 1;
                    }
                };

                match unsafe { fork() } {
                    Ok(ForkResult::Child) => {
                        signal::reset_child_signals();
                        let pid = nix::unistd::getpid();
                        setpgid(pid, pid).ok();

                        // Child: set up pipes
                        let read_fd = read_from_parent.into_raw_fd();
                        let write_fd = write_to_parent.into_raw_fd();

                        // Close parent's ends
                        close(write_to_child.into_raw_fd()).ok();
                        close(read_from_child.into_raw_fd()).ok();

                        // Redirect stdin from parent's write
                        dup2_raw(read_fd, 0).ok();
                        close(read_fd).ok();

                        // Redirect stdout to parent's read
                        dup2_raw(write_fd, 1).ok();
                        close(write_fd).ok();

                        // Execute the command
                        let code = execute_simple(command, state);
                        child_exit(code);
                    }
                    Ok(ForkResult::Parent { child }) => {
                        // Parent: save pipe fds in array variable
                        let coproc_var = name.clone().unwrap_or_else(|| "COPROC".to_string());

                        // Close child's ends
                        close(read_from_parent.into_raw_fd()).ok();
                        close(write_to_parent.into_raw_fd()).ok();

                        // Get raw fds for the pipes (still open)
                        let write_fd = write_to_child.into_raw_fd();
                        let read_fd = read_from_child.into_raw_fd();

                        // Set array: COPROC[0]=read_fd COPROC[1]=write_fd
                        let coproc_array = vec![read_fd.to_string(), write_fd.to_string()];
                        state.arrays.insert(coproc_var, coproc_array);

                        // Don't wait for coproc - it runs in background
                        state.last_bg_pid = Some(child.as_raw() as u32);
                        0
                    }
                    Err(_) => {
                        eprintln!("jsh: coproc: fork failed");
                        1
                    }
                }
            })
        }
    }
}

fn execute_command_list(cmds: &[CompleteCommand], state: &mut ShellState) -> i32 {
    if control_flow_requested(state) {
        return state.last_exit_code;
    }
    let mut code = 0;
    for cmd in cmds {
        let outcome = execute_complete_command_outcome(cmd, state);
        code = outcome.status;
        if control_flow_requested(state) {
            return code;
        }
        if errexit_active(state) && code != 0 && !outcome.failure_exempt {
            fire_err_trap(state);
            state.last_failure_exempt = outcome.failure_exempt;
            if control_flow_requested(state) {
                return code;
            }
            // An ERR trap may intentionally disable errexit before returning.
            if errexit_active(state) {
                state.errexit_status = Some(code);
                return code;
            }
            // The trap handled the failure and explicitly disabled errexit.
            // A surrounding direct compound must not report it a second time.
            state.last_failure_exempt = true;
        }
    }
    code
}

fn execute_condition(cmds: &[CompleteCommand], state: &mut ShellState) -> i32 {
    with_errexit_suppressed(state, |state| execute_command_list(cmds, state))
}

fn match_pattern(value: &str, pattern: &str, extglob: bool) -> bool {
    crate::glob_match::pattern_match(pattern, value, extglob)
}

/// True for a `[[ ... ]]` conditional, whose operands expand by different rules
/// than a normal command's arguments.
fn is_conditional_command(words: &[Word]) -> bool {
    // `[[` reaches the word parser as one part, but as a Glob rather than a
    // Literal: it opens with a bracket.
    matches!(
        words.first().map(|w| w.as_slice()),
        Some([WordPart::Literal(head)] | [WordPart::Glob(head)]) if head == "[["
    )
}

// --- Redirect handling ---

struct SavedFd {
    original_fd: RawFd,
    // None records an originally closed descriptor, which must be closed again.
    saved_fd: Option<OwnedFd>,
    original_flags: i32,
}

#[derive(Default)]
struct RedirectGuard {
    saved: Vec<Rc<RefCell<SavedFd>>>,
}

thread_local! {
    // Shell execution is single-threaded; forks inherit the currently active
    // frames. Weak slots let nested functions move any ancestor's backup
    // before a user redirect closes, replaces, or references that fd number.
    static REDIRECT_BACKUPS: RefCell<Vec<Weak<RefCell<SavedFd>>>> = const { RefCell::new(Vec::new()) };
}

fn protected_fd_reservations(reserved: &HashSet<RawFd>) -> HashSet<RawFd> {
    let mut all = reserved.clone();
    REDIRECT_BACKUPS.with(|registry| {
        registry.borrow_mut().retain(|entry| {
            if let Some(saved) = entry.upgrade() {
                all.insert(saved.borrow().original_fd);
                true
            } else {
                false
            }
        });
    });
    all
}

fn reserve_user_fd(fd: RawFd, reserved: &HashSet<RawFd>) -> io::Result<()> {
    let mut all = protected_fd_reservations(reserved);
    all.insert(fd);
    REDIRECT_BACKUPS.with(|registry| {
        for entry in registry.borrow().iter() {
            if let Some(slot) = entry.upgrade() {
                let mut saved = slot.borrow_mut();
                if saved
                    .saved_fd
                    .as_ref()
                    .is_some_and(|backup| backup.as_raw_fd() == fd)
                {
                    saved.saved_fd = Some(duplicate_private_fd(fd, &all)?);
                }
            }
        }
        Ok(())
    })
}

impl RedirectGuard {
    fn save(&mut self, fd: RawFd, reserved: &HashSet<RawFd>) -> io::Result<()> {
        if fd < 0 {
            return Err(io::Error::from_raw_os_error(nix::libc::EBADF));
        }
        if self
            .saved
            .iter()
            .any(|saved| saved.borrow().original_fd == fd)
        {
            return Ok(());
        }
        let flags = unsafe { nix::libc::fcntl(fd, nix::libc::F_GETFD) };
        let backup = if flags < 0 {
            let error = io::Error::last_os_error();
            if error.raw_os_error() != Some(nix::libc::EBADF) {
                return Err(error);
            }
            None
        } else {
            Some(duplicate_private_fd(
                fd,
                &protected_fd_reservations(reserved),
            )?)
        };
        let slot = Rc::new(RefCell::new(SavedFd {
            original_fd: fd,
            saved_fd: backup,
            original_flags: flags.max(0),
        }));
        REDIRECT_BACKUPS.with(|registry| registry.borrow_mut().push(Rc::downgrade(&slot)));
        self.saved.push(slot);
        Ok(())
    }

    /// `exec` without a program makes its successful redirections permanent.
    fn commit(&mut self) {
        self.saved.clear();
    }
}

impl Drop for RedirectGuard {
    fn drop(&mut self) {
        if !self.saved.is_empty() {
            // Rust stdout is buffered even when its underlying descriptor is
            // redirected. Drain it while the command's destination is active.
            let _ = io::stdout().flush();
            let _ = io::stderr().flush();
        }
        for slot in self.saved.drain(..).rev() {
            let mut saved = slot.borrow_mut();
            if let Some(backup) = saved.saved_fd.take() {
                if dup2_raw(backup.as_raw_fd(), saved.original_fd).is_ok() {
                    unsafe {
                        nix::libc::fcntl(
                            saved.original_fd,
                            nix::libc::F_SETFD,
                            saved.original_flags,
                        );
                    }
                }
            } else {
                let _ = close(saved.original_fd);
            }
        }
    }
}

fn duplicate_private_fd(fd: RawFd, reserved: &HashSet<RawFd>) -> io::Result<OwnedFd> {
    let mut minimum = 10;
    loop {
        let raw = unsafe { nix::libc::fcntl(fd, nix::libc::F_DUPFD_CLOEXEC, minimum) };
        if raw < 0 {
            return Err(io::Error::last_os_error());
        }
        let owned = unsafe { OwnedFd::from_raw_fd(raw) };
        if !reserved.contains(&raw) {
            return Ok(owned);
        }
        minimum = raw
            .checked_add(1)
            .ok_or_else(|| io::Error::from_raw_os_error(nix::libc::EMFILE))?;
    }
}

/// Helper to call dup2 with raw file descriptors.
fn dup2_raw(oldfd: RawFd, newfd: RawFd) -> nix::Result<()> {
    unsafe {
        match nix::libc::dup2(oldfd, newfd) {
            -1 => Err(nix::Error::last()),
            _ => Ok(()),
        }
    }
}

/// Materialize here-doc / here-string content into a private, immediately
/// unlinked file. A pipe would deadlock before execution for a large body.
fn heredoc_file(data: &str) -> io::Result<File> {
    use std::io::{Seek, SeekFrom};
    use std::sync::atomic::{AtomicU64, Ordering};
    static NEXT_HEREDOC: AtomicU64 = AtomicU64::new(0);
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_nanos();
    for _ in 0..128 {
        let serial = NEXT_HEREDOC.fetch_add(1, Ordering::Relaxed);
        let path = std::env::temp_dir().join(format!(
            "jsh-heredoc-{}-{nanos}-{serial}",
            std::process::id()
        ));
        let mut file = match OpenOptions::new()
            .read(true)
            .write(true)
            .create_new(true)
            .mode(0o600)
            .open(&path)
        {
            Ok(file) => file,
            Err(error) if error.kind() == io::ErrorKind::AlreadyExists => continue,
            Err(error) => return Err(error),
        };
        // Fail before writing sensitive here-doc bytes if unlinking failed.
        std::fs::remove_file(&path)?;
        file.write_all(data.as_bytes())?;
        file.seek(SeekFrom::Start(0))?;
        return Ok(file);
    }
    Err(io::Error::new(
        io::ErrorKind::AlreadyExists,
        "cannot allocate here-document",
    ))
}

fn open_output(path: &str, noclobber: bool) -> io::Result<File> {
    if !noclobber {
        return File::create(path);
    }
    match OpenOptions::new().write(true).create_new(true).open(path) {
        Ok(file) => Ok(file),
        Err(error) if error.kind() == io::ErrorKind::AlreadyExists => {
            // Open without O_TRUNC/O_CREAT, then check the actual opened inode.
            // Existing devices/FIFOs (including /dev/null) remain usable, while
            // a symlink or racing rename cannot make us truncate a regular file.
            let file = OpenOptions::new().write(true).open(path)?;
            if file.metadata()?.is_file() {
                Err(io::Error::new(
                    io::ErrorKind::AlreadyExists,
                    "cannot overwrite existing file",
                ))
            } else {
                Ok(file)
            }
        }
        Err(error) => Err(error),
    }
}

fn redirect_file(file: File, targets: &[RawFd]) -> io::Result<()> {
    let source = file.as_raw_fd();
    for &fd in targets {
        dup2_raw(source, fd).map_err(io::Error::from)?;
        if source == fd {
            // File::open uses CLOEXEC. dup2(fd, fd) leaves that flag intact,
            // so reopening a closed stdio fd would otherwise disappear at exec.
            if unsafe { nix::libc::fcntl(fd, nix::libc::F_SETFD, 0) } < 0 {
                return Err(io::Error::last_os_error());
            }
        }
    }
    if targets.contains(&source) {
        // This descriptor is now a user redirection, not an owned temporary.
        let _ = file.into_raw_fd();
    }
    Ok(())
}

fn duplicate_target(data: &str) -> io::Result<Option<RawFd>> {
    if data == "-" {
        return Ok(None);
    }
    if data.is_empty() || !data.bytes().all(|b| b.is_ascii_digit()) {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "bad file descriptor",
        ));
    }
    let fd = data
        .parse::<RawFd>()
        .map_err(|_| io::Error::new(io::ErrorKind::InvalidInput, "bad file descriptor"))?;
    Ok(Some(fd))
}

fn apply_one_redirect(
    kind: &RedirectKind,
    fd: RawFd,
    data: &str,
    noclobber: bool,
) -> io::Result<()> {
    match kind {
        RedirectKind::Output | RedirectKind::Clobber => redirect_file(
            open_output(data, noclobber && *kind != RedirectKind::Clobber)?,
            &[fd],
        ),
        RedirectKind::Append => redirect_file(
            OpenOptions::new().create(true).append(true).open(data)?,
            &[fd],
        ),
        RedirectKind::Input => redirect_file(File::open(data)?, &[fd]),
        RedirectKind::HereString | RedirectKind::HereDoc => {
            redirect_file(heredoc_file(data)?, &[fd])
        }
        RedirectKind::DupOutput | RedirectKind::DupInput => {
            match duplicate_target(data)? {
                None => {
                    if let Err(error) = close(fd) {
                        // Closing an already closed descriptor is legal.
                        if error != Errno::EBADF {
                            return Err(error.into());
                        }
                    }
                }
                Some(source) => dup2_raw(source, fd).map_err(io::Error::from)?,
            }
            Ok(())
        }
        RedirectKind::OutputAll => redirect_file(open_output(data, noclobber)?, &[1, 2]),
        RedirectKind::AppendAll => redirect_file(
            OpenOptions::new().create(true).append(true).open(data)?,
            &[1, 2],
        ),
    }
}

fn redirect_fd(redir: &Redirect) -> RawFd {
    match redir.kind {
        RedirectKind::Output
        | RedirectKind::Clobber
        | RedirectKind::Append
        | RedirectKind::DupOutput
        | RedirectKind::OutputAll
        | RedirectKind::AppendAll => redir.fd.unwrap_or(1),
        RedirectKind::Input
        | RedirectKind::HereString
        | RedirectKind::HereDoc
        | RedirectKind::DupInput => redir.fd.unwrap_or(0),
    }
}

fn redirect_targets(redir: &Redirect) -> Vec<RawFd> {
    if matches!(
        redir.kind,
        RedirectKind::OutputAll | RedirectKind::AppendAll
    ) {
        vec![1, 2]
    } else {
        vec![redirect_fd(redir)]
    }
}

fn redirect_error(data: &str, error: io::Error, state: &mut ShellState) -> i32 {
    let message = format!("{data}: {error}");
    // Respect earlier stderr redirects without panicking when stderr is closed.
    let _ = writeln!(std::io::stderr(), "jsh: {message}");
    state.set_error(message, 1);
    1
}

fn expand_redirect(redir: &Redirect, state: &mut ShellState) -> Result<String, i32> {
    if state.shell_opts.nounset {
        if let Some(param) = nounset_violation(std::slice::from_ref(&redir.target), state) {
            return Err(report_nounset(state, &param));
        }
    }
    let data = if redir.kind == RedirectKind::HereString {
        // No field splitting or pathname expansion; exactly one appended LF.
        format!("{}\n", expand_word_to_string(&redir.target, state))
    } else if let Some(options) = &redir.here_doc {
        if options.expand_vars {
            expand_word_to_string(&crate::parser::parse_word_parts(&options.content), state)
        } else {
            options.content.clone()
        }
    } else {
        let targets = expand_word(&redir.target, state);
        if let Some(code) = report_expansion_error(state) {
            return Err(code);
        }
        if targets.len() != 1 {
            return Err(redirect_error(
                "redirection",
                io::Error::new(io::ErrorKind::InvalidInput, "ambiguous redirect"),
                state,
            ));
        }
        targets.into_iter().next().unwrap()
    };
    if let Some(code) = report_expansion_error(state) {
        return Err(code);
    }
    Ok(data)
}

fn setup_redirects(redirects: &[Redirect], state: &mut ShellState) -> Result<RedirectGuard, i32> {
    let mut guard = RedirectGuard::default();
    let mut reserved: HashSet<RawFd> = redirects.iter().flat_map(redirect_targets).collect();
    if !redirects.is_empty() {
        let _ = io::stdout().flush();
        let _ = io::stderr().flush();
    }
    for redir in redirects {
        let data = expand_redirect(redir, state)?;
        let result = (|| -> io::Result<()> {
            if matches!(redir.kind, RedirectKind::DupOutput | RedirectKind::DupInput) {
                if let Some(source) = duplicate_target(&data)? {
                    reserved.insert(source);
                    reserve_user_fd(source, &reserved)?;
                }
            }
            for fd in redirect_targets(redir) {
                reserve_user_fd(fd, &reserved)?;
                guard.save(fd, &reserved)?;
            }
            apply_one_redirect(
                &redir.kind,
                redirect_fd(redir),
                &data,
                state.shell_opts.noclobber,
            )
        })();
        if let Err(error) = result {
            return Err(redirect_error(&data, error, state));
        }
    }
    Ok(guard)
}

fn apply_redirects_in_child(redirects: &[Redirect], state: &mut ShellState) -> Result<(), i32> {
    let mut reserved: HashSet<RawFd> = redirects.iter().flat_map(redirect_targets).collect();
    for redir in redirects {
        let data = expand_redirect(redir, state)?;
        let result = (|| -> io::Result<()> {
            if matches!(redir.kind, RedirectKind::DupOutput | RedirectKind::DupInput) {
                if let Some(source) = duplicate_target(&data)? {
                    reserved.insert(source);
                    reserve_user_fd(source, &reserved)?;
                }
            }
            for fd in redirect_targets(redir) {
                reserve_user_fd(fd, &reserved)?;
            }
            apply_one_redirect(
                &redir.kind,
                redirect_fd(redir),
                &data,
                state.shell_opts.noclobber,
            )
        })();
        if let Err(error) = result {
            return Err(redirect_error(&data, error, state));
        }
    }
    Ok(())
}

#[cfg(test)]
mod output_tests {
    use super::*;

    #[test]
    fn initial_foreground_stop_status_preserves_the_actual_signal() {
        let mut child = std::process::Command::new("/bin/sh")
            .args(["-c", "kill -STOP $$"])
            .spawn()
            .expect("spawn stopping child");
        let pid = Pid::from_raw(child.id() as i32);
        let mut state = ShellState::new(false);
        let status = wait_for_fg(pid, &mut state);
        let stopped = state.jobs.jobs.first().map(|job| job.status.clone());
        let _ = child.kill();
        let _ = child.wait();

        assert_eq!(status, 128 + nix::sys::signal::Signal::SIGSTOP as i32);
        assert_eq!(stopped, Some(crate::job::JobStatus::Stopped));
    }

    #[test]
    fn color_requires_a_terminal_and_no_color_must_be_absent() {
        assert!(should_use_color(true, false));
        assert!(!should_use_color(false, false));
        assert!(!should_use_color(true, true));
    }

    #[test]
    fn redirected_errors_and_hints_are_plain_text() {
        let error = format_shell_error("ech: command not found", false);
        let hint = format_command_hint("echo", false);

        assert_eq!(error, "jsh: ech: command not found");
        assert_eq!(hint, "       did you mean 'echo'?");
        assert!(!error.contains('\x1b'));
        assert!(!hint.contains('\x1b'));
        assert!(format_command_hint("echo", true).contains('\x1b'));
    }
}
