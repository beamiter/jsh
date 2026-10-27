//! The `kill` builtin and the signal name table it shares with anything that
//! needs bash's spelling of signal names.

use crate::environment::ShellState;
use nix::libc;

/// Signals with fixed numbers, as bash names them (without the `SIG` prefix).
/// Real-time signals are computed, since their range is decided at run time.
const NAMED_SIGNALS: &[(libc::c_int, &str)] = &[
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
    #[cfg(any(target_os = "linux", target_os = "android"))]
    (libc::SIGSTKFLT, "STKFLT"),
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
    #[cfg(any(target_os = "linux", target_os = "android"))]
    (libc::SIGPWR, "PWR"),
    (libc::SIGSYS, "SYS"),
];

/// One past the largest signal number.
#[cfg(any(target_os = "linux", target_os = "android"))]
fn signal_limit() -> libc::c_int {
    libc::SIGRTMAX() + 1
}

#[cfg(not(any(target_os = "linux", target_os = "android")))]
fn signal_limit() -> libc::c_int {
    NAMED_SIGNALS.iter().map(|(n, _)| *n).max().unwrap_or(31) + 1
}

#[cfg(any(target_os = "linux", target_os = "android"))]
fn realtime_range() -> Option<(libc::c_int, libc::c_int)> {
    Some((libc::SIGRTMIN(), libc::SIGRTMAX()))
}

#[cfg(not(any(target_os = "linux", target_os = "android")))]
fn realtime_range() -> Option<(libc::c_int, libc::c_int)> {
    None
}

/// Bash's name for signal `number`, without `SIG`: `KILL`, `RTMIN+3`,
/// `RTMAX-1`. Signal 0 is `EXIT`. `None` for numbers with no name (32 and 33
/// are reserved by the C library on Linux).
pub(crate) fn signal_name(number: libc::c_int) -> Option<String> {
    if number == 0 {
        return Some("EXIT".to_string());
    }
    if let Some((_, name)) = NAMED_SIGNALS.iter().find(|(n, _)| *n == number) {
        return Some((*name).to_string());
    }
    let (min, max) = realtime_range()?;
    if !(min..=max).contains(&number) {
        return None;
    }
    // Bash counts up from RTMIN for the lower half and down from RTMAX above.
    Some(if number == min {
        "RTMIN".to_string()
    } else if number == max {
        "RTMAX".to_string()
    } else if number - min <= (max - min) / 2 {
        format!("RTMIN+{}", number - min)
    } else {
        format!("RTMAX-{}", max - number)
    })
}

/// A signal given by number or by name (any case, `SIG` prefix optional).
pub(crate) fn decode_signal(spec: &str) -> Option<libc::c_int> {
    if !spec.is_empty() && spec.bytes().all(|b| b.is_ascii_digit()) {
        let number: libc::c_int = spec.parse().ok()?;
        return (number < signal_limit()).then_some(number);
    }
    let upper = spec.to_ascii_uppercase();
    let name = upper.strip_prefix("SIG").unwrap_or(&upper);
    if name == "EXIT" {
        return Some(0);
    }
    if let Some((number, _)) = NAMED_SIGNALS.iter().find(|(_, n)| *n == name) {
        return Some(*number);
    }
    let (min, max) = realtime_range()?;
    let number = if name == "RTMIN" {
        min
    } else if name == "RTMAX" {
        max
    } else if let Some(offset) = name.strip_prefix("RTMIN+") {
        min.checked_add(offset.parse().ok()?)?
    } else {
        let offset = name.strip_prefix("RTMAX-")?;
        max.checked_sub(offset.parse().ok()?)?
    };
    (min..=max).contains(&number).then_some(number)
}

/// `kill -l` with no operands: every signal, five to a line, as bash lays
/// them out.
fn print_signal_table() {
    let mut out = String::new();
    let mut column = 0;
    for number in 1..signal_limit() {
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
    print!("{out}");
}

/// `kill -l SPEC...`: names for numbers (an exit status above 128 names the
/// signal that caused it) and numbers for names.
fn list_signals(specs: &[String]) -> i32 {
    if specs.is_empty() {
        print_signal_table();
        return 0;
    }
    let mut status = 0;
    for spec in specs {
        if let Ok(mut number) = spec.parse::<i64>() {
            if number > 128 {
                number -= 128;
            }
            match libc::c_int::try_from(number)
                .ok()
                .filter(|n| (0..signal_limit()).contains(n))
            {
                Some(number) => {
                    if let Some(name) = signal_name(number) {
                        println!("{name}");
                    }
                }
                None => {
                    eprintln!("jsh: kill: {spec}: invalid signal specification");
                    status = 1;
                }
            }
        } else {
            match decode_signal(spec) {
                Some(number) => println!("{number}"),
                None => {
                    eprintln!("jsh: kill: {spec}: invalid signal specification");
                    status = 1;
                }
            }
        }
    }
    status
}

const KILL_USAGE: &str =
    "kill: usage: kill [-s sigspec | -n signum | -sigspec] pid | jobspec ... or kill -l [sigspec]";

pub(super) fn builtin_kill(args: &[String], state: &mut ShellState) -> i32 {
    let mut signal = libc::SIGTERM;
    let mut signal_spec = String::from("TERM");
    let mut saw_signal = false;
    let mut index = 0;
    while let Some(word) = args.get(index) {
        match word.as_str() {
            "-l" | "-L" => return list_signals(&args[index + 1..]),
            "-s" | "-n" => {
                let Some(spec) = args.get(index + 1) else {
                    eprintln!("jsh: kill: {word}: option requires an argument");
                    return 1;
                };
                signal_spec = spec.clone();
                saw_signal = true;
                index += 2;
                match decode_signal(spec) {
                    Some(number) => signal = number,
                    None => {
                        eprintln!("jsh: kill: {spec}: invalid signal specification");
                        return 1;
                    }
                }
            }
            "--" => {
                index += 1;
                break;
            }
            // Only the first `-SPEC` names the signal; a later `-N` is a
            // process group.
            spec if spec.starts_with('-') && spec.len() > 1 && !saw_signal => {
                let spec = &spec[1..];
                let spec = spec
                    .strip_prefix('s')
                    .filter(|rest| rest.starts_with(|c: char| c.is_ascii_alphabetic()))
                    .or_else(|| {
                        spec.strip_prefix('n')
                            .filter(|rest| rest.starts_with(|c: char| c.is_ascii_digit()))
                    })
                    .unwrap_or(spec);
                signal_spec = spec.to_string();
                saw_signal = true;
                index += 1;
                match decode_signal(spec) {
                    Some(number) => signal = number,
                    None => {
                        eprintln!("jsh: kill: {spec}: invalid signal specification");
                        return 1;
                    }
                }
            }
            _ => break,
        }
    }
    let targets = &args[index..];
    if targets.is_empty() {
        eprintln!("{KILL_USAGE}");
        return 2;
    }

    let mut any_succeeded = false;
    for target in targets {
        // A job's processes form one process group (the background child
        // calls setpgid on itself), so a job spec signals the whole group.
        let (pid, group) = if target.starts_with('%') {
            let Some(pid) = state
                .jobs
                .resolve_spec(target)
                .and_then(|id| state.jobs.get_by_id(id).map(|job| job.pid.as_raw()))
            else {
                eprintln!("jsh: kill: {target}: no such job");
                continue;
            };
            (pid, true)
        } else {
            match target.parse::<libc::pid_t>() {
                Ok(pid) => (pid, false),
                Err(_) => {
                    eprintln!("jsh: kill: {target}: arguments must be process or job IDs");
                    continue;
                }
            }
        };
        // SAFETY: kill(2) and killpg(2) take plain integers.
        let mut result = if group {
            unsafe { libc::killpg(pid, signal) }
        } else {
            unsafe { libc::kill(pid, signal) }
        };
        if result != 0 && group {
            // The child may not have reached setpgid yet.
            result = unsafe { libc::kill(pid, signal) };
        }
        if result == 0 {
            any_succeeded = true;
            continue;
        }
        let error = std::io::Error::last_os_error();
        if error.raw_os_error() == Some(libc::EINVAL) {
            eprintln!("jsh: kill: {signal_spec}: invalid signal specification");
        } else {
            eprintln!("jsh: kill: ({pid}) - {}", super::os_error_text(&error));
        }
    }
    if any_succeeded {
        0
    } else {
        1
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn names_and_numbers_round_trip() {
        assert_eq!(decode_signal("KILL"), Some(9));
        assert_eq!(decode_signal("sigterm"), Some(15));
        assert_eq!(decode_signal("int"), Some(2));
        assert_eq!(decode_signal("0"), Some(0));
        assert_eq!(decode_signal("EXIT"), Some(0));
        assert_eq!(decode_signal("NOPE"), None);
        assert_eq!(decode_signal("999"), None);
        assert_eq!(signal_name(9).as_deref(), Some("KILL"));
        assert_eq!(signal_name(0).as_deref(), Some("EXIT"));
        for number in 1..signal_limit() {
            if let Some(name) = signal_name(number) {
                assert_eq!(decode_signal(&name), Some(number), "{name}");
            }
        }
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn realtime_signals_use_bash_names() {
        let min = libc::SIGRTMIN();
        let max = libc::SIGRTMAX();
        assert_eq!(signal_name(min).as_deref(), Some("RTMIN"));
        assert_eq!(signal_name(min + 1).as_deref(), Some("RTMIN+1"));
        assert_eq!(signal_name(max - 1).as_deref(), Some("RTMAX-1"));
        assert_eq!(signal_name(max).as_deref(), Some("RTMAX"));
        assert_eq!(signal_name(32), None);
    }
}
