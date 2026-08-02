/// AST executor: fork/exec, pipes, redirects, compound commands.
use crate::builtins;
use crate::environment::ShellState;
use crate::expand::{expand_word, expand_word_to_string, expand_words};
use crate::parser::ast::*;
use crate::signal;

use nix::errno::Errno;
use nix::sys::wait::{waitpid, WaitPidFlag, WaitStatus};
use nix::unistd::{close, execvp, fork, pipe, setpgid, tcsetpgrp, ForkResult, Pid};
use std::ffi::CString;
use std::fs::{File, OpenOptions};
use std::io::{IsTerminal, Write};
use std::os::unix::io::{AsRawFd, BorrowedFd, IntoRawFd, OwnedFd, RawFd};

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
    for name in builtins::BUILTIN_NAMES {
        consider(&mut best, name);
    }
    // Phase 15d: include value-aware signed builtins (where/each/try/...).
    for name in crate::signature::SIGNATURES.keys() {
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
        Ok(WaitStatus::Stopped(_, _)) => {
            let cmd_str = format!("(pid {})", pid);
            let jid = state.jobs.add(pid, cmd_str.clone());
            if let Some(job) = state.jobs.get_by_id(jid) {
                job.status = crate::job::JobStatus::Stopped;
                eprintln!("\n[{}]+  Stopped                    {}", jid, cmd_str);
            }
            148
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

        if last != 0 && !outcome.failure_exempt {
            fire_err_trap(state);
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
    exit_requested()
        || signal::pending_status().is_some()
        || state.abort_current_program
        || state.return_requested
        || state.loop_break
        || state.loop_continue
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

fn unset_parameter(name: &str, state: &ShellState) -> Option<String> {
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
    outcome
}

fn execute_and_or(list: &AndOrList, state: &mut ShellState) -> CommandOutcome {
    let first_is_conditional = list.first.negated || !list.rest.is_empty();
    let mut code = execute_pipeline_in_context(&list.first, state, first_is_conditional);
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
        failure_exempt: final_pipeline.negated || last_executed < list.rest.len(),
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
            Err(code) => return code,
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

fn execute_pipeline(pipeline: &Pipeline, state: &mut ShellState) -> i32 {
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
    if cmds.iter().all(is_value_aware_command) {
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

    let mut prev_read_fd: Option<RawFd> = None;
    let mut child_pids: Vec<Pid> = Vec::new();
    let mut pgid = Pid::from_raw(0);

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

        state
            .fork_count
            .fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        match unsafe { fork() } {
            Ok(ForkResult::Child) => {
                signal::reset_child_signals();
                state.interactive = false;
                let my_pid = nix::unistd::getpid();
                let target_pgid = if pgid.as_raw() == 0 { my_pid } else { pgid };
                setpgid(my_pid, target_pgid).ok();

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

                let code = execute_command_in_pipeline_child(cmd, state);
                child_exit(code);
            }
            Ok(ForkResult::Parent { child }) => {
                let first_child = pgid.as_raw() == 0;
                if first_child {
                    pgid = child;
                }
                setpgid(child, pgid).ok();
                if first_child {
                    signal::set_foreground_pgid(Some(pgid.as_raw()));
                }
                child_pids.push(child);
                if let Some(fd) = write_fd {
                    close(fd).ok();
                }
                if let Some(fd) = prev_read_fd {
                    close(fd).ok();
                }
                prev_read_fd = read_fd;
            }
            Err(e) => {
                signal::set_foreground_pgid(None);
                eprintln!("jsh: fork failed: {}", e);
                return 1;
            }
        }
    }

    let shell_pgid = nix::unistd::getpgrp();
    if state.interactive && pgid.as_raw() != 0 {
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
    signal::set_foreground_pgid(None);

    if state.interactive {
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
        Command::Simple(simple) => execute_simple(simple, state),
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
        // Indexed assignment: arr[idx]=value
        let value = expand_word_to_string(&assign.value, state);
        state.set_array_element(&assign.name, index, &value);
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

    // Phase 14c hot path: the literal-value JSON and pure-expression
    // shortcuts don't need to touch `state.let_vars` at all — they read
    // captures from a local map. Avoiding the take+clone+restore here
    // saves a sizeable allocation per `each`/`where`/`reduce` iteration.
    let trimmed = closure.body_src.trim();
    if !trimmed.is_empty() {
        if let Ok(j) = serde_json::from_str::<serde_json::Value>(trimmed) {
            return Ok(Value::from_json(j));
        }
    }

    // Build a lightweight var view: captured + params + $in. Cloning captured
    // is unavoidable (try_eval may need the values), but we skip the broader
    // state.let_vars swap unless we fall through to the shell path.
    let mut local_vars: HashMap<String, Value> = closure.captured.clone();
    for (i, p) in closure.params.iter().enumerate() {
        let v = args.get(i).cloned().unwrap_or(Value::Null);
        local_vars.insert(p.clone(), v);
    }
    if !local_vars.contains_key("in") {
        local_vars.insert(
            "in".to_string(),
            args.first().cloned().unwrap_or(Value::Null),
        );
    }

    // Expression-body shortcut: `{|a, b| $a + $b}`, `{|r| if $r.x > 5 ... }`,
    // etc. Runs BEFORE installing into state so the bulk-iteration case
    // (each / where / reduce) avoids per-iteration state churn.
    match crate::closure_expr::try_eval(&closure.body_src, &local_vars) {
        Ok(Some(v)) => return Ok(v),
        Ok(None) => {} // not a pure expression — fall through to shell path
        Err(msg) => {
            state.set_error(msg, 1);
            return Err(1);
        }
    }

    // Fall through to the shell-command path: now we need state.let_vars to
    // hold the local view so variable resolution inside the parsed body works.
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
                    data = f(data, extra_args, state)?;
                }
                let data = match data {
                    PipelineData::Stream(it) => PipelineData::Values(it.collect()),
                    other => other,
                };
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
        let mut traced = Vec::new();
        for assign in &cmd.assignments {
            traced.push(execute_assignment(assign, state));
        }
        if state.shell_opts.xtrace {
            eprintln!("{}{}", xtrace_prefix(state), traced.join(" "));
        }
        return state.last_command_sub_status.take().unwrap_or(0);
    }

    // Phase 5b: nushell-style `let NAME = EXPR`.
    // Disambiguated from bash arithmetic `let "x = 1+2"` by requiring `=` as a
    // separate, bare word (no quoting). We access raw WordParts here so a
    // closure literal `{|x|...}` reaches build_value_from_words intact.
    if is_typed_let(&cmd.words) {
        return execute_typed_let(&cmd.words, state);
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
        return 0;
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
        let saved_fds = setup_redirects(&cmd.redirects, state);
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
        restore_fds(saved_fds);

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

    // Check for builtin
    if builtins::is_builtin(cmd_name) {
        let saved_fds = setup_redirects(&cmd.redirects, state);
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

        let code = builtins::run_builtin(cmd_name, args, state);

        for (name, old) in saved_vars {
            match old {
                Some(v) => state.set_var(&name, &v),
                None => state.unset_var(&name),
            }
        }
        restore_fds(saved_fds);
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
        apply_redirects_in_child(&cmd.redirects, state);

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

            apply_redirects_in_child(&cmd.redirects, state);

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
    let saved = setup_redirects(redirects, state);
    let result = f(state);
    restore_fds(saved);
    result
}

pub fn execute_compound(cmd: &CompoundCommand, state: &mut ShellState) -> i32 {
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
                apply_redirects_in_child(redirects, state);
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
                if state.abort_current_program {
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

                    if exit_requested() || state.abort_current_program || state.return_requested {
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
                // Execute init expression
                if !init.is_empty() {
                    let _ = crate::expand::expand_arithmetic(init, state);
                }

                let mut code = 0;
                let _ = code;
                state.loop_depth += 1;
                loop {
                    // Check condition
                    let cond_result = if condition.is_empty() {
                        // Empty condition means infinite loop (like for ((;;)))
                        true
                    } else {
                        let cond_str = crate::expand::expand_arithmetic(condition, state);
                        cond_str.parse::<i32>().unwrap_or(0) != 0
                    };

                    if !cond_result {
                        break;
                    }

                    // Execute body
                    code = execute_command_list(body, state);

                    if exit_requested() || state.abort_current_program || state.return_requested {
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
                        let _ = crate::expand::expand_arithmetic(update, state);
                    }
                }
                state.loop_depth -= 1;

                code
            })
        }
        CompoundCommand::While {
            condition,
            body,
            redirects,
        } => with_redirects(redirects, state, |state| {
            let mut code = 0;
            state.loop_depth += 1;
            loop {
                let cond = execute_condition(condition, state);
                if state.abort_current_program {
                    code = cond;
                    break;
                }
                if cond != 0 {
                    break;
                }
                code = execute_command_list(body, state);
                if exit_requested() || state.abort_current_program || state.return_requested {
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
            code
        }),
        CompoundCommand::Until {
            condition,
            body,
            redirects,
        } => with_redirects(redirects, state, |state| {
            let mut code = 0;
            state.loop_depth += 1;
            loop {
                let cond = execute_condition(condition, state);
                if state.abort_current_program {
                    code = cond;
                    break;
                }
                if cond == 0 {
                    break;
                }
                code = execute_command_list(body, state);
                if exit_requested() || state.abort_current_program || state.return_requested {
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
                        if exit_requested() || state.abort_current_program || state.return_requested
                        {
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

                                    if exit_requested()
                                        || state.abort_current_program
                                        || state.return_requested
                                    {
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
                // Evaluate the arithmetic expression
                let result_str = crate::expand::expand_arithmetic(expr, state);
                if let Ok(result) = result_str.parse::<i32>() {
                    // In bash, (( expr )) returns 0 if expr != 0, and 1 if expr == 0
                    // This is the opposite of C - expressions are checked for truthiness
                    if result != 0 {
                        0
                    } else {
                        1
                    }
                } else {
                    1
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
            return code;
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
    saved_fd: OwnedFd,
}

/// Helper to call dup2 with raw file descriptors
fn dup2_raw(oldfd: RawFd, newfd: RawFd) -> nix::Result<()> {
    unsafe {
        match nix::libc::dup2(oldfd, newfd) {
            -1 => Err(nix::Error::last()),
            _ => Ok(()),
        }
    }
}

/// Materialize here-doc / here-string content into a temp file and return a read
/// fd positioned at the start. Avoids the pipe-buffer deadlock for large content.
fn heredoc_fd(data: &str) -> Option<RawFd> {
    use std::io::{Seek, SeekFrom};
    let mut dir = std::env::temp_dir();
    let pid = std::process::id();
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_nanos())
        .unwrap_or(0);
    dir.push(format!("jsh-heredoc-{}-{}", pid, nanos));

    let mut file = OpenOptions::new()
        .read(true)
        .write(true)
        .create_new(true)
        .open(&dir)
        .ok()?;
    // Unlink immediately; the open fd keeps the file alive until closed.
    std::fs::remove_file(&dir).ok();
    file.write_all(data.as_bytes()).ok()?;
    file.seek(SeekFrom::Start(0)).ok()?;
    Some(file.into_raw_fd())
}

fn apply_one_redirect(
    kind: &RedirectKind,
    fd: RawFd,
    data: &str,
    _here_doc_opts: &Option<HereDocOptions>,
) {
    match kind {
        RedirectKind::Output => {
            if let Ok(file) = File::create(data) {
                let src = file.into_raw_fd();
                dup2_raw(src, fd).ok();
                if src != fd {
                    close(src).ok();
                }
            }
        }
        RedirectKind::Append => {
            if let Ok(file) = OpenOptions::new().create(true).append(true).open(data) {
                let src = file.into_raw_fd();
                dup2_raw(src, fd).ok();
                if src != fd {
                    close(src).ok();
                }
            }
        }
        RedirectKind::Input => {
            if let Ok(file) = File::open(data) {
                let src = file.into_raw_fd();
                dup2_raw(src, fd).ok();
                if src != fd {
                    close(src).ok();
                }
            }
        }
        RedirectKind::HereString | RedirectKind::HereDoc => {
            // Use a temp file rather than a pipe: a blocking write into a pipe with
            // no reader deadlocks once the content exceeds the pipe buffer (~64KB).
            if let Some(src) = heredoc_fd(data) {
                dup2_raw(src, fd).ok();
                close(src).ok();
            }
        }
        RedirectKind::DupOutput | RedirectKind::DupInput => {
            if data == "-" {
                close(fd).ok();
            } else if let Ok(target_fd) = data.parse::<RawFd>() {
                dup2_raw(target_fd, fd).ok();
            }
        }
        RedirectKind::OutputAll => {
            // &> redirects both stdout (1) and stderr (2) to the file
            if let Ok(file) = File::create(data) {
                let src = file.into_raw_fd();
                dup2_raw(src, 1).ok(); // stdout
                dup2_raw(src, 2).ok(); // stderr
                if src != 1 && src != 2 {
                    close(src).ok();
                }
            }
        }
        RedirectKind::AppendAll => {
            // &>> appends both stdout (1) and stderr (2) to the file
            if let Ok(file) = OpenOptions::new().create(true).append(true).open(data) {
                let src = file.into_raw_fd();
                dup2_raw(src, 1).ok(); // stdout
                dup2_raw(src, 2).ok(); // stderr
                if src != 1 && src != 2 {
                    close(src).ok();
                }
            }
        }
    }
}

fn redirect_fd(redir: &Redirect) -> RawFd {
    match redir.kind {
        RedirectKind::Output
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

fn setup_redirects(redirects: &[Redirect], state: &mut ShellState) -> Vec<SavedFd> {
    let mut saved = Vec::new();
    for redir in redirects {
        let data = if let Some(here_doc_opts) = &redir.here_doc {
            if here_doc_opts.expand_vars {
                expand_word_to_string(
                    &crate::parser::parse_word_parts(&here_doc_opts.content),
                    state,
                )
            } else {
                here_doc_opts.content.clone()
            }
        } else {
            expand_word_to_string(&redir.target, state)
        };

        // For OutputAll and AppendAll, we need to save both stdout and stderr
        match redir.kind {
            RedirectKind::OutputAll | RedirectKind::AppendAll => {
                // Save both fd 1 (stdout) and fd 2 (stderr)
                unsafe {
                    if let Ok(sfd1) = nix::unistd::dup(BorrowedFd::borrow_raw(1)) {
                        saved.push(SavedFd {
                            original_fd: 1,
                            saved_fd: sfd1,
                        });
                    }
                    if let Ok(sfd2) = nix::unistd::dup(BorrowedFd::borrow_raw(2)) {
                        saved.push(SavedFd {
                            original_fd: 2,
                            saved_fd: sfd2,
                        });
                    }
                }
                apply_one_redirect(&redir.kind, 1, &data, &redir.here_doc);
            }
            _ => {
                let fd = redirect_fd(redir);
                // Safe because fd is a valid file descriptor at this point
                unsafe {
                    if let Ok(sfd) = nix::unistd::dup(BorrowedFd::borrow_raw(fd)) {
                        saved.push(SavedFd {
                            original_fd: fd,
                            saved_fd: sfd,
                        });
                    }
                }
                apply_one_redirect(&redir.kind, fd, &data, &redir.here_doc);
            }
        }
    }
    saved
}

fn restore_fds(saved: Vec<SavedFd>) {
    for s in saved.into_iter().rev() {
        dup2_raw(s.saved_fd.as_raw_fd(), s.original_fd).ok();
        // OwnedFd will be dropped here and close the fd
    }
}

fn apply_redirects_in_child(redirects: &[Redirect], state: &mut ShellState) {
    for redir in redirects {
        let fd = redirect_fd(redir);

        // Use here-doc content if available, otherwise expand target
        let data = if let Some(here_doc_opts) = &redir.here_doc {
            // Optionally expand variables if requested
            if here_doc_opts.expand_vars {
                expand_word_to_string(
                    &crate::parser::parse_word_parts(&here_doc_opts.content),
                    state,
                )
            } else {
                here_doc_opts.content.clone()
            }
        } else {
            expand_word_to_string(&redir.target, state)
        };

        apply_one_redirect(&redir.kind, fd, &data, &redir.here_doc);
    }
}

#[cfg(test)]
mod output_tests {
    use super::*;

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
