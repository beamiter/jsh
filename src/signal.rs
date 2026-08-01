/// Signal handling for the shell.
use nix::libc;
use nix::sys::signal::{self, SaFlags, SigAction, SigHandler, SigSet, Signal};
use std::sync::atomic::{AtomicBool, AtomicI32, Ordering};

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
/// status 0.
pub fn take_pending_status() -> Option<i32> {
    if SIGINT_RECEIVED.swap(false, Ordering::SeqCst) {
        return Some(128 + libc::SIGINT);
    }
    let signal = TERMINATION_SIGNAL.load(Ordering::SeqCst);
    (signal != 0).then_some(128 + signal)
}

/// Inspect, but do not consume, a pending terminating signal.
pub fn pending_status() -> Option<i32> {
    if SIGINT_RECEIVED.load(Ordering::SeqCst) {
        return Some(128 + libc::SIGINT);
    }
    let signal = TERMINATION_SIGNAL.load(Ordering::SeqCst);
    (signal != 0).then_some(128 + signal)
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
pub fn reset_child_signals() {
    unsafe {
        let sa_default = SigAction::new(SigHandler::SigDfl, SaFlags::empty(), SigSet::empty());
        signal::sigaction(Signal::SIGINT, &sa_default).ok();
        signal::sigaction(Signal::SIGHUP, &sa_default).ok();
        signal::sigaction(Signal::SIGTERM, &sa_default).ok();
        signal::sigaction(Signal::SIGTSTP, &sa_default).ok();
        signal::sigaction(Signal::SIGTTIN, &sa_default).ok();
        signal::sigaction(Signal::SIGTTOU, &sa_default).ok();
        signal::sigaction(Signal::SIGPIPE, &sa_default).ok();
        signal::sigaction(Signal::SIGCHLD, &sa_default).ok();
    }
}
