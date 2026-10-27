/// Signal handling for the shell.
use nix::libc;
use nix::sys::signal::{self, SaFlags, SigAction, SigHandler, SigSet, SigmaskHow, Signal};
use nix::sys::wait::{waitpid, WaitPidFlag, WaitStatus};
use nix::unistd::Pid;
use std::sync::atomic::{AtomicBool, AtomicI32, AtomicU64, Ordering};

pub static SIGCHLD_RECEIVED: AtomicBool = AtomicBool::new(false);
pub static SIGWINCH_RECEIVED: AtomicBool = AtomicBool::new(false);
pub static SIGINT_RECEIVED: AtomicBool = AtomicBool::new(false);
pub static SIGHUP_RECEIVED: AtomicBool = AtomicBool::new(false);
pub static TERMINATION_SIGNAL: AtomicI32 = AtomicI32::new(0);
static FOREGROUND_PGID: AtomicI32 = AtomicI32::new(0);

fn forward_to_foreground(signal: libc::c_int) {
    let pgid = FOREGROUND_PGID.load(Ordering::SeqCst);
    if pgid > 0 {
        // `kill` is async-signal-safe. The shell itself is not a member of the
        // foreground child's process group, so this cannot recurse back into
        // the shell handler.
        unsafe {
            libc::kill(-pgid, signal);
        }
    }
}

extern "C" fn sigchld_handler(_: libc::c_int) {
    SIGCHLD_RECEIVED.store(true, Ordering::SeqCst);
}

extern "C" fn sigwinch_handler(_: libc::c_int) {
    SIGWINCH_RECEIVED.store(true, Ordering::SeqCst);
}

extern "C" fn sigint_handler(_: libc::c_int) {
    SIGINT_RECEIVED.store(true, Ordering::SeqCst);
    forward_to_foreground(libc::SIGINT);
}

extern "C" fn sighup_handler(_: libc::c_int) {
    SIGHUP_RECEIVED.store(true, Ordering::SeqCst);
    TERMINATION_SIGNAL.store(libc::SIGHUP, Ordering::SeqCst);
    forward_to_foreground(libc::SIGHUP);
}

extern "C" fn sigterm_handler(_: libc::c_int) {
    SIGHUP_RECEIVED.store(true, Ordering::SeqCst);
    TERMINATION_SIGNAL.store(libc::SIGTERM, Ordering::SeqCst);
    forward_to_foreground(libc::SIGTERM);
}

/// Highest signal number a trap can name: Linux numbers its real-time
/// signals up to 64, and one bit per signal must fit in a `u64` mask.
pub const MAX_TRAP_SIGNAL: i32 = 64;

/// Per-signal "a trapped signal arrived" flags, consumed at the executor's
/// safe points. Only atomics are touched in the handler, so it stays
/// async-signal-safe however much shell code the trap action later runs.
static TRAP_PENDING: [AtomicBool; MAX_TRAP_SIGNAL as usize + 1] =
    [const { AtomicBool::new(false) }; MAX_TRAP_SIGNAL as usize + 1];
/// Summary of `TRAP_PENDING`, so a safe point with nothing to do costs one load.
static ANY_TRAP_PENDING: AtomicBool = AtomicBool::new(false);
/// Bit `n - 1` set: `trap '' n` is in force. Children keep ignoring these signals
/// (bash's rule, and how `nohup`-style protection reaches exec'd programs),
/// so `reset_child_signals` must not hand them back their default action.
static USER_IGNORED: AtomicU64 = AtomicU64::new(0);

fn signal_bit(signal: i32) -> u64 {
    if (1..=MAX_TRAP_SIGNAL).contains(&signal) {
        1u64 << (signal - 1)
    } else {
        0
    }
}

extern "C" fn trap_handler(signal: libc::c_int, info: *mut libc::siginfo_t, _: *mut libc::c_void) {
    if (1..=MAX_TRAP_SIGNAL).contains(&signal) {
        TRAP_PENDING[signal as usize].store(true, Ordering::SeqCst);
        ANY_TRAP_PENDING.store(true, Ordering::SeqCst);
    }
    match signal {
        // Trapping these must not starve the shell's own bookkeeping.
        libc::SIGCHLD => SIGCHLD_RECEIVED.store(true, Ordering::SeqCst),
        libc::SIGWINCH => SIGWINCH_RECEIVED.store(true, Ordering::SeqCst),
        libc::SIGINT | libc::SIGQUIT | libc::SIGHUP | libc::SIGTERM => {
            // In bash a terminal's ^C reaches the shell and its foreground
            // job together, because both share a process group. jsh runs
            // children in their own groups and forwards instead, but only a
            // kernel-generated signal (positive `si_code`): one sent with
            // kill(2) was addressed to the shell alone, and bash's trap then
            // waits for the foreground command rather than killing it.
            //
            // SAFETY: an SA_SIGINFO handler receives a valid `siginfo_t`.
            let from_kernel = !info.is_null() && unsafe { (*info).si_code } > 0;
            if from_kernel {
                forward_to_foreground(signal);
            }
        }
        _ => {}
    }
}

/// A signal disposition as the kernel stores it. Raw `libc` rather than
/// `nix::sys::signal::Signal`, which cannot name real-time signals.
pub type Disposition = libc::sigaction;

/// Install `handler` (a function address, `SIG_IGN` or `SIG_DFL`) for
/// `signal`, returning the disposition it replaced.
fn set_disposition(
    signal: i32,
    handler: libc::sighandler_t,
    flags: libc::c_int,
) -> std::io::Result<Disposition> {
    if !(1..=MAX_TRAP_SIGNAL).contains(&signal) {
        return Err(std::io::Error::from_raw_os_error(libc::EINVAL));
    }
    // SAFETY: a zeroed sigaction is valid once its mask is initialised; the
    // installed handler is async-signal-safe (atomics and kill(2) only).
    unsafe {
        let mut action: libc::sigaction = std::mem::zeroed();
        action.sa_sigaction = handler;
        action.sa_flags = flags;
        libc::sigemptyset(&mut action.sa_mask);
        let mut previous: libc::sigaction = std::mem::zeroed();
        if libc::sigaction(signal, &action, &mut previous) != 0 {
            return Err(std::io::Error::last_os_error());
        }
        Ok(previous)
    }
}

/// Route `signal` to the trap machinery. Returns the disposition it replaced
/// so `trap - SIG` can restore exactly what the shell had before. Fails for
/// SIGKILL and SIGSTOP, which cannot be caught.
pub fn catch_for_trap(signal: i32) -> std::io::Result<Disposition> {
    let mut flags = libc::SA_SIGINFO | libc::SA_RESTART;
    if signal == libc::SIGCHLD {
        flags |= libc::SA_NOCLDSTOP;
    }
    let handler = trap_handler as *const () as libc::sighandler_t;
    let previous = set_disposition(signal, handler, flags)?;
    USER_IGNORED.fetch_and(!signal_bit(signal), Ordering::SeqCst);
    Ok(previous)
}

/// `trap '' SIG`: ignore the signal here and in every child.
pub fn ignore_for_trap(signal: i32) -> std::io::Result<Disposition> {
    let previous = set_disposition(signal, libc::SIG_IGN, 0)?;
    USER_IGNORED.fetch_or(signal_bit(signal), Ordering::SeqCst);
    // A signal that arrived while it was still caught must not run the trap
    // the user has just replaced.
    TRAP_PENDING[signal as usize].store(false, Ordering::SeqCst);
    Ok(previous)
}

/// Put back the disposition a trap replaced (`trap - SIG`, or a subshell
/// dropping its parent's traps).
pub fn restore_after_trap(signal: i32, previous: &Disposition) {
    if !(1..=MAX_TRAP_SIGNAL).contains(&signal) {
        return;
    }
    // SAFETY: `previous` is a disposition sigaction(2) returned earlier.
    unsafe {
        libc::sigaction(signal, previous, std::ptr::null_mut());
    }
    USER_IGNORED.fetch_and(!signal_bit(signal), Ordering::SeqCst);
    TRAP_PENDING[signal as usize].store(false, Ordering::SeqCst);
}

/// True when `trap '' signal` is in force.
pub fn is_user_ignored(signal: i32) -> bool {
    USER_IGNORED.load(Ordering::SeqCst) & signal_bit(signal) != 0
}

/// True when some trapped signal is waiting for its action to run.
pub fn trap_pending() -> bool {
    ANY_TRAP_PENDING.load(Ordering::SeqCst)
}

/// Take every pending trapped signal, lowest number first (bash's order).
pub fn take_pending_traps() -> Vec<i32> {
    if !ANY_TRAP_PENDING.swap(false, Ordering::SeqCst) {
        return Vec::new();
    }
    (1..=MAX_TRAP_SIGNAL)
        .filter(|&signal| TRAP_PENDING[signal as usize].swap(false, Ordering::SeqCst))
        .collect()
}

/// Mark `signal` pending again: a trap that could not run yet (another trap
/// action is still executing) keeps its place in the queue.
pub fn requeue_trap(signal: i32) {
    if (1..=MAX_TRAP_SIGNAL).contains(&signal) {
        TRAP_PENDING[signal as usize].store(true, Ordering::SeqCst);
        ANY_TRAP_PENDING.store(true, Ordering::SeqCst);
    }
}

fn first_pending_trap() -> Option<i32> {
    if !ANY_TRAP_PENDING.load(Ordering::SeqCst) {
        return None;
    }
    (1..=MAX_TRAP_SIGNAL).find(|&signal| TRAP_PENDING[signal as usize].load(Ordering::SeqCst))
}

/// How a `wait` for one child ended.
#[derive(Debug)]
pub enum WaitOutcome {
    /// waitpid's own answer: the child's status, or an error such as ECHILD.
    Done(nix::Result<WaitStatus>),
    /// A trapped signal arrived first. Bash's `wait` then returns 128+N and
    /// the trap runs; the signal stays pending for the executor to dispatch.
    Trapped(i32),
    /// A terminating signal is latched; the caller should unwind.
    Terminating(i32),
}

/// Wait for `pid` like waitpid(2), but give up as soon as a trapped signal
/// arrives, as bash's `wait` builtin does.
///
/// Trap handlers use SA_RESTART so foreground commands are never cut short;
/// this wait therefore cannot rely on EINTR. Instead every signal is blocked,
/// the child and the pending flags are polled, and `sigsuspend` atomically
/// unblocks and sleeps: a signal landing between the poll and the sleep is
/// delivered inside `sigsuspend` and wakes it, so none is lost. SIGCHLD
/// wakes it when a child changes state.
pub fn wait_interruptibly(pid: Pid) -> WaitOutcome {
    let Ok(old_mask) = SigSet::thread_get_mask() else {
        return WaitOutcome::Done(waitpid(pid, None));
    };
    // A subshell resets SIGCHLD to its default, which discards the signal
    // without waking `sigsuspend`. Catch it for the duration of the wait.
    let chld = SigAction::new(
        SigHandler::Handler(sigchld_handler),
        SaFlags::SA_RESTART | SaFlags::SA_NOCLDSTOP,
        SigSet::empty(),
    );
    // SAFETY: the handler only stores to an atomic.
    let saved_chld = unsafe { signal::sigaction(Signal::SIGCHLD, &chld) }.ok();
    if let Some(previous) = saved_chld {
        if matches!(previous.handler(), SigHandler::SigAction(_)) {
            // A trap on CHLD is already installed; keep its handler.
            // SAFETY: restoring the disposition that was just replaced.
            let _ = unsafe { signal::sigaction(Signal::SIGCHLD, &previous) };
        }
    }
    let _ = signal::pthread_sigmask(SigmaskHow::SIG_BLOCK, Some(&SigSet::all()), None);
    let mut sleep_mask = old_mask;
    sleep_mask.remove(Signal::SIGCHLD);

    let outcome = loop {
        match waitpid(pid, Some(WaitPidFlag::WNOHANG)) {
            Ok(WaitStatus::StillAlive) => {}
            other => break WaitOutcome::Done(other),
        }
        if let Some(signal) = first_pending_trap() {
            break WaitOutcome::Trapped(signal);
        }
        if let Some(status) = pending_status() {
            break WaitOutcome::Terminating(status);
        }
        // Returns (with EINTR) once any unblocked signal has been handled.
        let _ = sleep_mask.suspend();
    };

    let _ = signal::pthread_sigmask(SigmaskHow::SIG_SETMASK, Some(&old_mask), None);
    if let Some(previous) = saved_chld {
        if !matches!(previous.handler(), SigHandler::SigAction(_)) {
            // SAFETY: restoring the disposition that was just replaced.
            let _ = unsafe { signal::sigaction(Signal::SIGCHLD, &previous) };
        }
    }
    outcome
}

pub fn set_foreground_pgid(pgid: Option<i32>) {
    FOREGROUND_PGID.store(pgid.unwrap_or(0), Ordering::SeqCst);
}

pub fn reset_pending_signals() {
    SIGINT_RECEIVED.store(false, Ordering::SeqCst);
    SIGHUP_RECEIVED.store(false, Ordering::SeqCst);
    TERMINATION_SIGNAL.store(0, Ordering::SeqCst);
    set_foreground_pgid(None);
}

/// Return a pending signal's shell exit status.
///
/// Interactive `SIGINT` is consumable because it cancels only the current
/// prompt or program. `SIGHUP` and `SIGTERM` are process-termination requests:
/// keep their status latched until the next explicit signal reset so an early
/// executor check cannot accidentally turn a later orderly shutdown into exit
/// status 0. A latched termination also takes precedence over a pending
/// interrupt; consuming the interrupt must not hide process shutdown.
pub fn take_pending_status() -> Option<i32> {
    let interrupted = SIGINT_RECEIVED.swap(false, Ordering::SeqCst);
    let signal = TERMINATION_SIGNAL.load(Ordering::SeqCst);
    if signal != 0 {
        Some(128 + signal)
    } else {
        interrupted.then_some(128 + libc::SIGINT)
    }
}

/// Inspect, but do not consume, a pending terminating signal.
pub fn pending_status() -> Option<i32> {
    let signal = TERMINATION_SIGNAL.load(Ordering::SeqCst);
    if signal != 0 {
        Some(128 + signal)
    } else {
        SIGINT_RECEIVED
            .load(Ordering::SeqCst)
            .then_some(128 + libc::SIGINT)
    }
}

pub fn install_shell_signals() {
    unsafe {
        let sa_ignore = SigAction::new(SigHandler::SigIgn, SaFlags::SA_RESTART, SigSet::empty());

        // Shell ignores these (children will get default)
        signal::sigaction(Signal::SIGTSTP, &sa_ignore).ok();
        signal::sigaction(Signal::SIGTTIN, &sa_ignore).ok();
        signal::sigaction(Signal::SIGTTOU, &sa_ignore).ok();
        signal::sigaction(Signal::SIGPIPE, &sa_ignore).ok();
        // Handle SIGHUP to trigger graceful shutdown (save session, then exit).
        let sa_hup = SigAction::new(
            SigHandler::Handler(sighup_handler),
            SaFlags::SA_RESTART,
            SigSet::empty(),
        );
        signal::sigaction(Signal::SIGHUP, &sa_hup).ok();

        // Handle SIGTERM (abnormal PTY termination) same as SIGHUP
        let sa_term = SigAction::new(
            SigHandler::Handler(sigterm_handler),
            SaFlags::SA_RESTART,
            SigSet::empty(),
        );
        signal::sigaction(Signal::SIGTERM, &sa_term).ok();

        // Custom handlers
        let sa_chld = SigAction::new(
            SigHandler::Handler(sigchld_handler),
            SaFlags::SA_RESTART | SaFlags::SA_NOCLDSTOP,
            SigSet::empty(),
        );
        signal::sigaction(Signal::SIGCHLD, &sa_chld).ok();

        let sa_winch = SigAction::new(
            SigHandler::Handler(sigwinch_handler),
            SaFlags::SA_RESTART,
            SigSet::empty(),
        );
        signal::sigaction(Signal::SIGWINCH, &sa_winch).ok();

        let sa_int = SigAction::new(
            SigHandler::Handler(sigint_handler),
            SaFlags::SA_RESTART,
            SigSet::empty(),
        );
        signal::sigaction(Signal::SIGINT, &sa_int).ok();
    }
}

/// Install shell handlers without `SA_RESTART` for signals that must wake a
/// noninteractive shell blocked while reading its program from stdin.
pub fn install_noninteractive_signals() {
    install_shell_signals();
    unsafe {
        let empty = SigSet::empty();
        let sigint = SigAction::new(SigHandler::Handler(sigint_handler), SaFlags::empty(), empty);
        let sighup = SigAction::new(SigHandler::Handler(sighup_handler), SaFlags::empty(), empty);
        let sigterm = SigAction::new(
            SigHandler::Handler(sigterm_handler),
            SaFlags::empty(),
            empty,
        );
        signal::sigaction(Signal::SIGINT, &sigint).ok();
        signal::sigaction(Signal::SIGHUP, &sighup).ok();
        signal::sigaction(Signal::SIGTERM, &sigterm).ok();
    }
}

/// Reset signal handlers to default (called in child after fork).
///
/// A signal the user ignored with `trap '' SIG` stays ignored: bash passes
/// SIG_IGN on to every child, which is what lets `trap '' HUP` protect the
/// programs a script starts.
pub fn reset_child_signals() {
    let ignored = USER_IGNORED.load(Ordering::SeqCst);
    unsafe {
        let sa_default = SigAction::new(SigHandler::SigDfl, SaFlags::empty(), SigSet::empty());
        let sa_ignore = SigAction::new(SigHandler::SigIgn, SaFlags::empty(), SigSet::empty());
        for sig in [
            Signal::SIGINT,
            Signal::SIGHUP,
            Signal::SIGTERM,
            Signal::SIGTSTP,
            Signal::SIGTTIN,
            Signal::SIGTTOU,
            Signal::SIGPIPE,
            Signal::SIGCHLD,
        ] {
            let action = if ignored & signal_bit(sig as i32) != 0 {
                &sa_ignore
            } else {
                &sa_default
            };
            signal::sigaction(sig, action).ok();
        }
    }
}
