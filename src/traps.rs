//! `trap`: the trap table, signal dispositions, and running trap actions.
//!
//! Actions live in `ShellState::traps`, keyed by canonical name: `EXIT`,
//! a signal name without its `SIG` prefix (`INT`, `USR1`, `RTMIN+3`), or one
//! of the shell conditions `DEBUG`, `ERR` and `RETURN`. Session snapshots
//! persist that map, so the keys stay plain strings. The empty action means
//! "ignore". Everything about *running* traps is in [`TrapRuntime`].
//!
//! A caught signal only sets a flag (see `signal::trap_handler`); its action
//! runs at the executor's next safe point — between commands, after a
//! foreground command returns, or when `wait` is interrupted — exactly the
//! points at which bash runs it.

use crate::environment::ShellState;
use crate::signal;
use nix::libc;
use std::collections::HashMap;
use std::io::Write;

/// What a trap is attached to. The derived order is bash's listing order.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub enum TrapTarget {
    Exit,
    Signal(i32),
    Debug,
    Err,
    Return,
}

impl TrapTarget {
    /// Key in `ShellState::traps`.
    pub fn key(self) -> String {
        match self {
            TrapTarget::Exit => "EXIT".to_string(),
            TrapTarget::Signal(number) => signal_name(number).unwrap_or_default(),
            TrapTarget::Debug => "DEBUG".to_string(),
            TrapTarget::Err => "ERR".to_string(),
            TrapTarget::Return => "RETURN".to_string(),
        }
    }

    /// Name as `trap -p` prints it.
    fn display_name(self) -> String {
        match self {
            TrapTarget::Signal(number) => format!("SIG{}", self.key_for(number)),
            other => other.key(),
        }
    }

    fn key_for(self, number: i32) -> String {
        signal_name(number).unwrap_or_else(|| number.to_string())
    }
}

/// The fixed (non-real-time) signals, numbered as on this platform.
fn named_signals() -> Vec<(i32, &'static str)> {
    let mut table = vec![
        (libc::SIGHUP, "HUP"),
        (libc::SIGINT, "INT"),
        (libc::SIGQUIT, "QUIT"),
        (libc::SIGILL, "ILL"),
        (libc::SIGTRAP, "TRAP"),
        (libc::SIGABRT, "ABRT"),
        (libc::SIGBUS, "BUS"),
        (libc::SIGFPE, "FPE"),
        (libc::SIGKILL, "KILL"),
        (libc::SIGUSR1, "USR1"),
        (libc::SIGSEGV, "SEGV"),
        (libc::SIGUSR2, "USR2"),
        (libc::SIGPIPE, "PIPE"),
        (libc::SIGALRM, "ALRM"),
        (libc::SIGTERM, "TERM"),
        (libc::SIGCHLD, "CHLD"),
        (libc::SIGCONT, "CONT"),
        (libc::SIGSTOP, "STOP"),
        (libc::SIGTSTP, "TSTP"),
        (libc::SIGTTIN, "TTIN"),
        (libc::SIGTTOU, "TTOU"),
        (libc::SIGURG, "URG"),
        (libc::SIGXCPU, "XCPU"),
        (libc::SIGXFSZ, "XFSZ"),
        (libc::SIGVTALRM, "VTALRM"),
        (libc::SIGPROF, "PROF"),
        (libc::SIGWINCH, "WINCH"),
        (libc::SIGIO, "IO"),
        (libc::SIGSYS, "SYS"),
    ];
    #[cfg(any(target_os = "linux", target_os = "android"))]
    {
        table.push((libc::SIGSTKFLT, "STKFLT"));
        table.push((libc::SIGPWR, "PWR"));
    }
    table
}

/// The real-time signal range, when the platform has one.
fn realtime_range() -> Option<(i32, i32)> {
    #[cfg(any(target_os = "linux", target_os = "android"))]
    {
        let (min, max) = (libc::SIGRTMIN(), libc::SIGRTMAX());
        if min <= max && max <= signal::MAX_TRAP_SIGNAL {
            return Some((min, max));
        }
    }
    None
}

/// Name of signal `number` without the `SIG` prefix, as bash spells it
/// (`RTMIN+3`, `RTMAX-2` for real-time signals).
pub fn signal_name(number: i32) -> Option<String> {
    if let Some((_, name)) = named_signals().into_iter().find(|(n, _)| *n == number) {
        return Some(name.to_string());
    }
    let (min, max) = realtime_range()?;
    if !(min..=max).contains(&number) {
        return None;
    }
    // Bash names the lower half from RTMIN up and the upper half from RTMAX
    // down.
    let from_min = number - min;
    Some(if from_min <= (max - min) / 2 {
        match from_min {
            0 => "RTMIN".to_string(),
            n => format!("RTMIN+{n}"),
        }
    } else {
        match max - number {
            0 => "RTMAX".to_string(),
            n => format!("RTMAX-{n}"),
        }
    })
}

fn signal_number(name: &str) -> Option<i32> {
    let name = match name {
        "CLD" => "CHLD",
        "IOT" => "ABRT",
        "POLL" => "IO",
        other => other,
    };
    if let Some((number, _)) = named_signals().into_iter().find(|(_, n)| *n == name) {
        return Some(number);
    }
    let (min, max) = realtime_range()?;
    let number = match name {
        "RTMIN" => min,
        "RTMAX" => max,
        _ => match name.strip_prefix("RTMIN+") {
            Some(offset) => min.checked_add(offset.parse::<i32>().ok()?)?,
            None => max.checked_sub(name.strip_prefix("RTMAX-")?.parse::<i32>().ok()?)?,
        },
    };
    (min..=max).contains(&number).then_some(number)
}

/// Parse a `trap` condition: a signal name (any case, `SIG` prefix
/// optional), a signal number, `0`/`EXIT`, `DEBUG`, `ERR` or `RETURN`.
pub fn parse_target(spec: &str) -> Option<TrapTarget> {
    if !spec.is_empty() && spec.bytes().all(|b| b.is_ascii_digit()) {
        let number: i32 = spec.parse().ok()?;
        return match number {
            0 => Some(TrapTarget::Exit),
            _ => signal_name(number).map(|_| TrapTarget::Signal(number)),
        };
    }
    let upper = spec.to_ascii_uppercase();
    match upper.as_str() {
        "EXIT" => return Some(TrapTarget::Exit),
        "DEBUG" => return Some(TrapTarget::Debug),
        "ERR" => return Some(TrapTarget::Err),
        "RETURN" => return Some(TrapTarget::Return),
        _ => {}
    }
    let name = upper.strip_prefix("SIG").unwrap_or(&upper);
    signal_number(name).map(TrapTarget::Signal)
}

/// Key in `ShellState::traps` for a trapped signal.
fn signal_key(number: i32) -> String {
    TrapTarget::Signal(number).key()
}

/// Trap bookkeeping that is not part of the persisted trap table.
#[derive(Default)]
pub struct TrapRuntime {
    /// Dispositions that `trap` replaced, so `trap - SIG` restores exactly
    /// what the shell had (its own INT/TERM handlers, SIG_IGN for PIPE...).
    saved_dispositions: HashMap<i32, signal::Disposition>,
    /// Trap actions currently executing, of any kind. While one runs,
    /// `BASH_COMMAND` keeps naming the interrupted command.
    pub running: usize,
    /// Bit `n - 1` set: the trap for signal `n` is executing. Another
    /// signal's trap may run inside it, as in bash; the same signal waits
    /// until its own action has finished.
    running_signals: u64,
    /// DEBUG, ERR or RETURN action executing; none re-enters itself.
    in_debug_trap: bool,
    in_err_trap: bool,
    in_return_trap: bool,
    /// The traps a subshell reset on entry. Bash keeps showing them to
    /// `trap`/`trap -p` until the subshell changes a trap, which is what
    /// makes `saved=$(trap -p INT)` work.
    inherited_display: Option<HashMap<String, String>>,
    /// A pipeline parent already ran the DEBUG trap for the simple command
    /// this child is about to execute.
    pub skip_next_debug: bool,
}

fn shell_flag(state: &ShellState, name: &str) -> bool {
    state
        .shell_opts
        .tracked_opts
        .get(name)
        .copied()
        .unwrap_or(false)
}

/// Install, ignore or reset one trap. `action` None means "reset".
fn set_trap(state: &mut ShellState, target: TrapTarget, action: Option<&str>) {
    let key = target.key();
    let TrapTarget::Signal(number) = target else {
        match action {
            Some(action) => {
                state.traps.insert(key, action.to_string());
            }
            None => {
                state.traps.remove(&key);
            }
        }
        return;
    };
    match action {
        None => {
            state.traps.remove(&key);
            if let Some(previous) = state.trap_runtime.saved_dispositions.remove(&number) {
                signal::restore_after_trap(number, &previous);
            }
        }
        Some(action) => {
            let installed = if action.is_empty() {
                signal::ignore_for_trap(number)
            } else {
                signal::catch_for_trap(number)
            };
            // SIGKILL and SIGSTOP cannot be caught; bash accepts the
            // command and nothing changes.
            if let Ok(previous) = installed {
                state
                    .trap_runtime
                    .saved_dispositions
                    .entry(number)
                    .or_insert(previous);
                state.traps.insert(key, action.to_string());
            }
        }
    }
}

const TRAP_USAGE: &str = "trap: usage: trap [-lp] [[arg] signal_spec ...]";

/// The `trap` builtin.
pub fn builtin_trap(args: &[String], state: &mut ShellState) -> i32 {
    let mut args = args;
    let mut list = false;
    let mut print = false;
    while let Some(first) = args.first() {
        match first.as_str() {
            "--" => {
                args = &args[1..];
                break;
            }
            option if option.len() > 1 && option.starts_with('-') => {
                for flag in option[1..].chars() {
                    match flag {
                        'l' => list = true,
                        'p' => print = true,
                        _ => {
                            eprintln!("jsh: trap: -{flag}: invalid option");
                            eprintln!("{TRAP_USAGE}");
                            return 2;
                        }
                    }
                }
                args = &args[1..];
            }
            _ => break,
        }
    }
    if list {
        print_signal_list();
        return 0;
    }
    if print || args.is_empty() {
        return print_traps(state, args);
    }

    // `trap SIG...` with a single operand, or with a leading unsigned number
    // (POSIX), resets each condition; `trap - SIG...` does the same.
    let numeric = !args[0].is_empty() && args[0].bytes().all(|b| b.is_ascii_digit());
    let (action, specs) = if args.len() == 1 || numeric || args[0] == "-" {
        let specs = if args[0] == "-" { &args[1..] } else { args };
        (None, specs)
    } else {
        (Some(args[0].as_str()), &args[1..])
    };
    let mut status = 0;
    for spec in specs {
        match parse_target(spec) {
            Some(target) => {
                state.trap_runtime.inherited_display = None;
                set_trap(state, target, action);
            }
            None => {
                eprintln!("jsh: trap: {spec}: invalid signal specification");
                status = 1;
            }
        }
    }
    status
}

/// Quote an action the way `trap -p` does, so its output can be re-read.
fn quote_action(action: &str) -> String {
    format!("'{}'", action.replace('\'', "'\\''"))
}

fn print_traps(state: &ShellState, specs: &[String]) -> i32 {
    let mut shown: Vec<(TrapTarget, &String)> = Vec::new();
    let tables = [
        Some(&state.traps),
        state.trap_runtime.inherited_display.as_ref(),
    ];
    for (key, action) in tables.into_iter().flatten().flat_map(|table| table.iter()) {
        if let Some(target) = parse_target(key) {
            if !shown.iter().any(|(t, _)| *t == target) {
                shown.push((target, action));
            }
        }
    }
    shown.sort_by_key(|(target, _)| *target);

    let stdout = std::io::stdout();
    let mut out = stdout.lock();
    let mut status = 0;
    let mut print = |target: TrapTarget, action: &str| {
        let _ = writeln!(
            out,
            "trap -- {} {}",
            quote_action(action),
            target.display_name()
        );
    };
    if specs.is_empty() {
        for (target, action) in &shown {
            print(*target, action);
        }
        return 0;
    }
    for spec in specs {
        match parse_target(spec) {
            Some(target) => {
                if let Some((_, action)) = shown.iter().find(|(t, _)| *t == target) {
                    print(target, action);
                }
            }
            None => {
                eprintln!("jsh: trap: {spec}: invalid signal specification");
                status = 1;
            }
        }
    }
    status
}

/// `trap -l`: every signal, five to a line, in bash's layout.
fn print_signal_list() {
    let mut out = String::new();
    let mut column = 0;
    for number in 1..=signal::MAX_TRAP_SIGNAL {
        let Some(name) = signal_name(number) else {
            continue;
        };
        out.push_str(&format!("{number:2}) SIG{name}"));
        column += 1;
        if column < 5 {
            out.push('\t');
        } else {
            out.push('\n');
            column = 0;
        }
    }
    if column != 0 {
        out.push('\n');
    }
    let _ = std::io::stdout().lock().write_all(out.as_bytes());
}

/// Run a trap action in the current shell. `$?` and the current line are
/// restored afterwards, so the interrupted program carries on as if nothing
/// happened — unless the action exits the shell.
pub fn run_trap_action(action: &str, state: &mut ShellState) {
    let Ok(commands) = crate::parser::parse(action) else {
        return;
    };
    let saved_status = state.last_exit_code;
    let saved_line = state.call_stack.lineno();
    let saved_exempt = state.last_failure_exempt;
    let nested_offset = state.call_stack.nested_line_offset();
    let saved_offset = std::mem::replace(&mut state.call_stack.line_offset, nested_offset);
    // A `break`, `continue` or `return` the interrupted code was carrying out
    // is not the action's to see. It resumes afterwards, unless the action
    // starts its own (`trap 'return 3' USR1` inside a function).
    let interrupted_flow = (
        std::mem::take(&mut state.loop_break),
        std::mem::take(&mut state.loop_continue),
        std::mem::take(&mut state.loop_unwind_levels),
        std::mem::take(&mut state.return_requested),
        std::mem::take(&mut state.return_value),
    );
    state.trap_runtime.running += 1;
    crate::executor::execute_trap_commands(&commands, state);
    state.trap_runtime.running -= 1;
    if !(state.loop_break || state.loop_continue || state.return_requested) {
        (
            state.loop_break,
            state.loop_continue,
            state.loop_unwind_levels,
            state.return_requested,
            state.return_value,
        ) = interrupted_flow;
    }
    state.call_stack.line_offset = saved_offset;
    state.call_stack.set_lineno(saved_line);
    state.last_failure_exempt = saved_exempt;
    if !crate::builtins::EXIT_REQUESTED.load(std::sync::atomic::Ordering::SeqCst) {
        state.last_exit_code = saved_status;
    }
}

/// Run the actions of trapped signals that have arrived. Called at safe
/// points; cheap when nothing is pending.
pub fn run_pending_traps(state: &mut ShellState) {
    if !signal::trap_pending() {
        return;
    }
    // Signals whose own action is already running up the stack: they run
    // again once it returns, not inside it.
    let mut deferred = Vec::new();
    'dispatch: loop {
        let pending = signal::take_pending_traps();
        if pending.is_empty() {
            break;
        }
        for number in pending {
            let bit = 1u64 << (number - 1);
            if state.trap_runtime.running_signals & bit != 0 {
                deferred.push(number);
                continue;
            }
            let action = match state.traps.get(&signal_key(number)) {
                Some(action) if !action.is_empty() => action.clone(),
                _ => continue,
            };
            state.trap_runtime.running_signals |= bit;
            run_trap_action(&action, state);
            state.trap_runtime.running_signals &= !bit;
            if crate::builtins::EXIT_REQUESTED.load(std::sync::atomic::Ordering::SeqCst)
                || signal::pending_status().is_some()
            {
                break 'dispatch;
            }
        }
    }
    for number in deferred {
        signal::requeue_trap(number);
    }
}

/// Run the ERR trap for a command that just failed, if one applies here.
pub fn run_err_trap(state: &mut ShellState) {
    if state.trap_runtime.in_err_trap {
        return;
    }
    let Some(action) = state.traps.get("ERR").filter(|a| !a.is_empty()).cloned() else {
        return;
    };
    state.trap_runtime.in_err_trap = true;
    run_trap_action(&action, state);
    state.trap_runtime.in_err_trap = false;
}

/// Run the DEBUG trap before a command. `BASH_COMMAND` and `$LINENO` must
/// already describe that command. Bash runs it inside other traps' actions
/// too, but never inside its own.
pub fn run_debug_trap(state: &mut ShellState) {
    if state.trap_runtime.in_debug_trap {
        return;
    }
    let Some(action) = state.traps.get("DEBUG").filter(|a| !a.is_empty()).cloned() else {
        return;
    };
    state.trap_runtime.in_debug_trap = true;
    run_trap_action(&action, state);
    state.trap_runtime.in_debug_trap = false;
}

/// Run the RETURN trap as a function or sourced file finishes, with `$?`
/// holding its status.
pub fn run_return_trap(state: &mut ShellState) {
    if state.trap_runtime.in_return_trap {
        return;
    }
    let Some(action) = state.traps.get("RETURN").filter(|a| !a.is_empty()).cloned() else {
        return;
    };
    state.trap_runtime.in_return_trap = true;
    run_trap_action(&action, state);
    state.trap_runtime.in_return_trap = false;
}

/// DEBUG, ERR and RETURN traps a function call set aside because the
/// function does not inherit them.
#[derive(Default)]
pub struct SavedFunctionTraps {
    traps: Vec<(&'static str, String)>,
}

/// Function entry: bash hides the DEBUG and RETURN traps from a function
/// unless `set -T` (functrace) is on, and the ERR trap unless `set -E`
/// (errtrace) is on.
pub fn enter_function(state: &mut ShellState) -> SavedFunctionTraps {
    let functrace = shell_flag(state, "functrace");
    let errtrace = shell_flag(state, "errtrace");
    let mut saved = SavedFunctionTraps::default();
    for (key, inherited) in [
        ("DEBUG", functrace),
        ("ERR", errtrace),
        ("RETURN", functrace),
    ] {
        if !inherited {
            if let Some(action) = state.traps.remove(key) {
                saved.traps.push((key, action));
            }
        }
    }
    saved
}

/// Function return: put the hidden traps back, except where the function set
/// its own — bash keeps a trap a function installed (`trap_if_untrapped`).
pub fn leave_function(state: &mut ShellState, saved: SavedFunctionTraps) {
    for (key, action) in saved.traps {
        state.traps.entry(key.to_string()).or_insert(action);
    }
}

/// Entering a subshell (`( )`, a pipeline stage, `&`, a command or process
/// substitution, a coprocess): caught signals go back to the dispositions
/// the shell had before trapping them, ignored ones stay ignored, the EXIT
/// trap is dropped, and ERR/DEBUG/RETURN survive only under errtrace or
/// functrace.
pub fn enter_subshell(state: &mut ShellState) {
    let errtrace = shell_flag(state, "errtrace");
    let functrace = shell_flag(state, "functrace");
    let mut reset: HashMap<String, String> = HashMap::new();
    let keys: Vec<String> = state.traps.keys().cloned().collect();
    for key in keys {
        let keep = match parse_target(&key) {
            Some(TrapTarget::Signal(_)) => state.traps.get(&key).is_some_and(|a| a.is_empty()),
            Some(TrapTarget::Err) => errtrace,
            Some(TrapTarget::Debug | TrapTarget::Return) => functrace,
            Some(TrapTarget::Exit) | None => false,
        };
        if !keep {
            if let Some(action) = state.traps.remove(&key) {
                reset.insert(key, action);
            }
        }
    }
    let saved = std::mem::take(&mut state.trap_runtime.saved_dispositions);
    for (number, previous) in saved {
        let ignored = state
            .traps
            .get(&signal_key(number))
            .is_some_and(|a| a.is_empty());
        if ignored {
            state
                .trap_runtime
                .saved_dispositions
                .insert(number, previous);
        } else {
            signal::restore_after_trap(number, &previous);
        }
    }
    // A signal that reached the parent before the fork is the parent's to
    // handle, not the subshell's.
    let _ = signal::take_pending_traps();
    if !reset.is_empty() {
        state.trap_runtime.inherited_display = Some(reset);
    }
    state.trap_runtime.running_signals = 0;
}

/// Re-install signal traps restored from a session snapshot, normalising
/// keys written by older versions (`SIGINT`, `10`).
pub fn rearm_restored_traps(state: &mut ShellState) {
    let restored: Vec<(String, String)> = state.traps.drain().collect();
    for (key, action) in restored {
        if let Some(target) = parse_target(&key) {
            set_trap(state, target, Some(&action));
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn conditions_parse_like_bash() {
        assert_eq!(parse_target("0"), Some(TrapTarget::Exit));
        assert_eq!(parse_target("exit"), Some(TrapTarget::Exit));
        assert_eq!(
            parse_target("SIGINT"),
            Some(TrapTarget::Signal(libc::SIGINT))
        );
        assert_eq!(parse_target("int"), Some(TrapTarget::Signal(libc::SIGINT)));
        assert_eq!(
            parse_target("sigusr1"),
            Some(TrapTarget::Signal(libc::SIGUSR1))
        );
        assert_eq!(parse_target("CLD"), Some(TrapTarget::Signal(libc::SIGCHLD)));
        assert_eq!(parse_target("15"), Some(TrapTarget::Signal(libc::SIGTERM)));
        assert_eq!(parse_target("Err"), Some(TrapTarget::Err));
        for bad in ["", "99", "FOO", "SIG", "-1", "RTMIN+99"] {
            assert_eq!(parse_target(bad), None, "{bad:?}");
        }
        assert!(TrapTarget::Exit < TrapTarget::Signal(1));
        assert!(TrapTarget::Signal(64) < TrapTarget::Debug);
    }

    #[cfg(any(target_os = "linux", target_os = "android"))]
    #[test]
    fn realtime_signals_use_bash_names() {
        let (min, max) = (libc::SIGRTMIN(), libc::SIGRTMAX());
        assert_eq!(signal_name(min).as_deref(), Some("RTMIN"));
        assert_eq!(signal_name(min + 3).as_deref(), Some("RTMIN+3"));
        assert_eq!(signal_name(max).as_deref(), Some("RTMAX"));
        assert_eq!(signal_name(max - 1).as_deref(), Some("RTMAX-1"));
        assert_eq!(
            parse_target("SIGRTMAX-1"),
            Some(TrapTarget::Signal(max - 1))
        );
        assert_eq!(
            TrapTarget::Signal(min + 3).display_name(),
            "SIGRTMIN+3".to_string()
        );
    }

    /// nix's `Signal` cannot name real-time signals; trapping must not
    /// depend on it.
    #[cfg(any(target_os = "linux", target_os = "android"))]
    #[test]
    fn realtime_signals_can_be_caught_and_restored() {
        let number = libc::SIGRTMIN() + 1;
        let previous = signal::catch_for_trap(number).expect("catch RTMIN+1");
        signal::restore_after_trap(number, &previous);
        assert!(signal::catch_for_trap(libc::SIGKILL).is_err());
    }

    #[test]
    fn actions_are_quoted_for_rereading() {
        assert_eq!(quote_action("echo 'q'"), r"'echo '\''q'\'''");
        assert_eq!(quote_action(""), "''");
    }
}
