//! Process limits and accounting: `umask`, `ulimit` and `times`. All three
//! act on the shell process itself, so the settings are inherited by every
//! command started afterwards (and stay inside a subshell that sets them).

use crate::environment::ShellState;
use nix::libc;
use nix::sys::stat::Mode;

// ============================================================
// umask
// ============================================================

/// The current file-creation mask. umask(2) can only be read by setting it, so
/// set it to a scratch value and immediately restore it; the shell is
/// single-threaded, so nothing can create a file in between.
fn current_umask() -> u32 {
    let mask = nix::sys::stat::umask(Mode::from_bits_truncate(0o022));
    nix::sys::stat::umask(mask);
    mask.bits() as u32 & 0o777
}

/// `u=rwx,g=rx,o=rx`: the permissions the mask lets through.
fn symbolic_umask(mask: u32) -> String {
    let allowed = !mask & 0o777;
    let class = |shift: u32| {
        let bits = (allowed >> shift) & 0o7;
        let mut text = String::new();
        for (bit, letter) in [(0o4, 'r'), (0o2, 'w'), (0o1, 'x')] {
            if bits & bit != 0 {
                text.push(letter);
            }
        }
        text
    };
    format!("u={},g={},o={}", class(6), class(3), class(0))
}

/// Apply a symbolic mode (`u=rwx,g-w,a+r`) to the permission bits a mask
/// allows, as bash's `parse_symbolic_mode` does: clauses are comma separated,
/// an omitted "who" means everyone, and only `r`, `w` and `x` are accepted.
fn apply_symbolic_mode(mode: &str, mut bits: u32) -> Result<u32, String> {
    const USER: u32 = 0o700;
    const GROUP: u32 = 0o070;
    const OTHER: u32 = 0o007;
    let mut chars = mode.chars().peekable();
    loop {
        let mut who = 0;
        while let Some(&c) = chars.peek() {
            who |= match c {
                'u' => USER,
                'g' => GROUP,
                'o' => OTHER,
                'a' => USER | GROUP | OTHER,
                _ => break,
            };
            chars.next();
        }
        let op = chars.next().unwrap_or('\0');
        if !matches!(op, '+' | '-' | '=') {
            return Err(format!("`{op}': invalid symbolic mode operator"));
        }
        let mut perm = 0;
        while let Some(&c) = chars.peek() {
            perm |= match c {
                'r' => 0o444,
                'w' => 0o222,
                'x' => 0o111,
                _ => break,
            };
            chars.next();
        }
        match chars.next() {
            None | Some(',') => {}
            Some(other) => return Err(format!("`{other}': invalid symbolic mode character")),
        }
        if who != 0 {
            perm &= who;
        }
        match op {
            '+' => bits |= perm,
            '-' => bits &= !perm,
            _ => {
                let who = if who == 0 { USER | GROUP | OTHER } else { who };
                bits = (bits & !who) | perm;
            }
        }
        if chars.peek().is_none() {
            return Ok(bits);
        }
    }
}

pub(super) fn builtin_umask(args: &[String], _state: &mut ShellState) -> i32 {
    let mut symbolic = false;
    let mut reusable = false;
    let mut index = 0;
    while let Some(arg) = args.get(index) {
        if arg == "--" {
            index += 1;
            break;
        }
        let Some(flags) = arg.strip_prefix('-').filter(|flags| !flags.is_empty()) else {
            break;
        };
        for flag in flags.chars() {
            match flag {
                'S' => symbolic = true,
                'p' => reusable = true,
                _ => {
                    eprintln!("jsh: umask: -{flag}: invalid option");
                    eprintln!("umask: usage: umask [-p] [-S] [mode]");
                    return 2;
                }
            }
        }
        index += 1;
    }

    let Some(mode) = args.get(index) else {
        let mask = current_umask();
        let text = if symbolic {
            symbolic_umask(mask)
        } else {
            format!("{mask:04o}")
        };
        match (reusable, symbolic) {
            (true, true) => println!("umask -S {text}"),
            (true, false) => println!("umask {text}"),
            _ => println!("{text}"),
        }
        return 0;
    };

    let mask = if mode.starts_with(|c: char| c.is_ascii_digit()) {
        match u32::from_str_radix(mode, 8) {
            Ok(mask) if mask <= 0o777 => mask,
            _ => {
                eprintln!("jsh: umask: {mode}: octal number out of range");
                return 1;
            }
        }
    } else {
        match apply_symbolic_mode(mode, !current_umask() & 0o777) {
            Ok(allowed) => !allowed & 0o777,
            Err(message) => {
                eprintln!("jsh: umask: {message}");
                return 1;
            }
        }
    };
    nix::sys::stat::umask(Mode::from_bits_truncate(mask as _));
    // `umask -S MODE` sets the mask and reports it symbolically.
    if symbolic {
        println!("{}", symbolic_umask(mask));
    }
    0
}

// ============================================================
// ulimit
// ============================================================

#[cfg(all(target_os = "linux", target_env = "gnu"))]
type Resource = libc::__rlimit_resource_t;
#[cfg(not(all(target_os = "linux", target_env = "gnu")))]
type Resource = libc::c_int;

/// Where a limit comes from: a real resource, or the pipe buffer size, which
/// bash reports (in 512-byte blocks) but which cannot be changed.
#[derive(Clone, Copy)]
enum Source {
    Resource(Resource),
    PipeSize,
}

struct Limit {
    option: char,
    description: &'static str,
    /// Bytes (or other base units) per unit shown to the user.
    factor: libc::rlim_t,
    units: Option<&'static str>,
    source: Source,
}

/// Bash's limit table, in bash's order, restricted to what this platform has.
/// `-c` and `-f` count 1024-byte blocks outside POSIX mode, as in bash.
const LIMITS: &[Limit] = &[
    #[cfg(target_os = "linux")]
    Limit {
        option: 'R',
        description: "real-time non-blocking time",
        factor: 1,
        units: Some("microseconds"),
        source: Source::Resource(libc::RLIMIT_RTTIME),
    },
    Limit {
        option: 'c',
        description: "core file size",
        factor: 1024,
        units: Some("blocks"),
        source: Source::Resource(libc::RLIMIT_CORE),
    },
    Limit {
        option: 'd',
        description: "data seg size",
        factor: 1024,
        units: Some("kbytes"),
        source: Source::Resource(libc::RLIMIT_DATA),
    },
    #[cfg(target_os = "linux")]
    Limit {
        option: 'e',
        description: "scheduling priority",
        factor: 1,
        units: None,
        source: Source::Resource(libc::RLIMIT_NICE),
    },
    Limit {
        option: 'f',
        description: "file size",
        factor: 1024,
        units: Some("blocks"),
        source: Source::Resource(libc::RLIMIT_FSIZE),
    },
    #[cfg(target_os = "linux")]
    Limit {
        option: 'i',
        description: "pending signals",
        factor: 1,
        units: None,
        source: Source::Resource(libc::RLIMIT_SIGPENDING),
    },
    Limit {
        option: 'l',
        description: "max locked memory",
        factor: 1024,
        units: Some("kbytes"),
        source: Source::Resource(libc::RLIMIT_MEMLOCK),
    },
    Limit {
        option: 'm',
        description: "max memory size",
        factor: 1024,
        units: Some("kbytes"),
        source: Source::Resource(libc::RLIMIT_RSS),
    },
    Limit {
        option: 'n',
        description: "open files",
        factor: 1,
        units: None,
        source: Source::Resource(libc::RLIMIT_NOFILE),
    },
    Limit {
        option: 'p',
        description: "pipe size",
        factor: 512,
        units: Some("512 bytes"),
        source: Source::PipeSize,
    },
    #[cfg(target_os = "linux")]
    Limit {
        option: 'q',
        description: "POSIX message queues",
        factor: 1,
        units: Some("bytes"),
        source: Source::Resource(libc::RLIMIT_MSGQUEUE),
    },
    #[cfg(target_os = "linux")]
    Limit {
        option: 'r',
        description: "real-time priority",
        factor: 1,
        units: None,
        source: Source::Resource(libc::RLIMIT_RTPRIO),
    },
    Limit {
        option: 's',
        description: "stack size",
        factor: 1024,
        units: Some("kbytes"),
        source: Source::Resource(libc::RLIMIT_STACK),
    },
    Limit {
        option: 't',
        description: "cpu time",
        factor: 1,
        units: Some("seconds"),
        source: Source::Resource(libc::RLIMIT_CPU),
    },
    Limit {
        option: 'u',
        description: "max user processes",
        factor: 1,
        units: None,
        source: Source::Resource(libc::RLIMIT_NPROC),
    },
    Limit {
        option: 'v',
        description: "virtual memory",
        factor: 1024,
        units: Some("kbytes"),
        source: Source::Resource(libc::RLIMIT_AS),
    },
    #[cfg(target_os = "linux")]
    Limit {
        option: 'x',
        description: "file locks",
        factor: 1,
        units: None,
        source: Source::Resource(libc::RLIMIT_LOCKS),
    },
];

const ULIMIT_USAGE: &str = "ulimit: usage: ulimit [-SHacdefilmnpqrstuvx] [limit]";

fn get_limit(limit: &Limit) -> std::io::Result<(libc::rlim_t, libc::rlim_t)> {
    match limit.source {
        Source::PipeSize => {
            let size = libc::PIPE_BUF as libc::rlim_t;
            Ok((size, size))
        }
        Source::Resource(resource) => {
            let mut value = libc::rlimit {
                rlim_cur: 0,
                rlim_max: 0,
            };
            // SAFETY: `value` is a valid, writable rlimit for the call.
            if unsafe { libc::getrlimit(resource, &mut value) } != 0 {
                return Err(std::io::Error::last_os_error());
            }
            Ok((value.rlim_cur, value.rlim_max))
        }
    }
}

fn set_limit(limit: &Limit, value: libc::rlim_t, soft: bool, hard: bool) -> std::io::Result<()> {
    let Source::Resource(resource) = limit.source else {
        return Err(std::io::Error::from_raw_os_error(libc::EINVAL));
    };
    let (mut current, mut maximum) = get_limit(limit)?;
    // Like bash, an unprivileged `ulimit -S x unlimited` means "as high as
    // the hard limit allows" rather than a request that is bound to fail.
    let value = if value == libc::RLIM_INFINITY
        && !hard
        && !nix::unistd::geteuid().is_root()
        && current <= maximum
    {
        maximum
    } else {
        value
    };
    if soft {
        current = value;
    }
    if hard {
        maximum = value;
    }
    let new = libc::rlimit {
        rlim_cur: current,
        rlim_max: maximum,
    };
    // SAFETY: `new` is a valid rlimit that outlives the call.
    if unsafe { libc::setrlimit(resource, &new) } != 0 {
        return Err(std::io::Error::last_os_error());
    }
    Ok(())
}

fn format_limit(limit: &Limit, value: libc::rlim_t, describe: bool) -> String {
    let mut line = String::new();
    if describe {
        let units = match limit.units {
            Some(units) => format!("({units}, -{}) ", limit.option),
            None => format!("(-{}) ", limit.option),
        };
        line.push_str(&format!("{:<20} {:>20}", limit.description, units));
    }
    if value == libc::RLIM_INFINITY {
        line.push_str("unlimited");
    } else {
        line.push_str(&(value / limit.factor).to_string());
    }
    line
}

pub(super) fn builtin_ulimit(args: &[String], _state: &mut ShellState) -> i32 {
    let mut soft = false;
    let mut hard = false;
    let mut all = false;
    // (limit, explicit value) in command-line order.
    let mut requests: Vec<(&Limit, Option<&str>)> = Vec::new();
    let mut index = 0;
    while let Some(arg) = args.get(index) {
        if arg == "--" {
            index += 1;
            break;
        }
        let Some(flags) = arg.strip_prefix('-').filter(|flags| !flags.is_empty()) else {
            break;
        };
        index += 1;
        for flag in flags.chars() {
            match flag {
                'S' => soft = true,
                'H' => hard = true,
                'a' => all = true,
                _ => match LIMITS.iter().find(|limit| limit.option == flag) {
                    Some(limit) => {
                        // An option takes the following word as its value
                        // unless that word is another option.
                        let value = args
                            .get(index)
                            .filter(|next| !next.starts_with('-') || next.len() == 1)
                            .map(String::as_str);
                        if value.is_some() {
                            index += 1;
                        }
                        requests.push((limit, value));
                    }
                    None => {
                        eprintln!("jsh: ulimit: -{flag}: invalid option");
                        eprintln!("{ULIMIT_USAGE}");
                        return 2;
                    }
                },
            }
        }
    }
    let operand = args.get(index).map(String::as_str);

    if all {
        for limit in LIMITS {
            match get_limit(limit) {
                Ok((current, maximum)) => {
                    let value = if soft || !hard { current } else { maximum };
                    println!("{}", format_limit(limit, value, true));
                }
                Err(error) => {
                    eprintln!(
                        "jsh: ulimit: {}: cannot get limit: {}",
                        limit.description,
                        super::os_error_text(&error)
                    );
                    return 1;
                }
            }
        }
        return 0;
    }

    // `ulimit` alone is `ulimit -f`, and `ulimit -n N` may also be written
    // with the value as a trailing operand.
    if requests.is_empty() {
        let file_size = LIMITS.iter().find(|limit| limit.option == 'f').unwrap();
        requests.push((file_size, operand));
    } else if let (Some(operand), Some(last)) = (operand, requests.last_mut()) {
        if last.1.is_none() {
            last.1 = Some(operand);
        }
    }

    let describe = requests.len() > 1;
    for (limit, value) in requests {
        let (current, maximum) = match get_limit(limit) {
            Ok(values) => values,
            Err(error) => {
                eprintln!(
                    "jsh: ulimit: {}: cannot get limit: {}",
                    limit.description,
                    super::os_error_text(&error)
                );
                return 1;
            }
        };
        let Some(value) = value else {
            let shown = if soft || !hard { current } else { maximum };
            println!("{}", format_limit(limit, shown, describe));
            continue;
        };
        let new = match value {
            "unlimited" => libc::RLIM_INFINITY,
            "hard" => maximum,
            "soft" => current,
            digits if !digits.is_empty() && digits.bytes().all(|b| b.is_ascii_digit()) => {
                match digits
                    .parse::<libc::rlim_t>()
                    .ok()
                    .and_then(|units| units.checked_mul(limit.factor))
                    .filter(|&bytes| bytes != libc::RLIM_INFINITY)
                {
                    Some(bytes) => bytes,
                    None => {
                        eprintln!("jsh: ulimit: {digits}: limit out of range");
                        return 1;
                    }
                }
            }
            other => {
                eprintln!("jsh: ulimit: {other}: invalid number");
                return 1;
            }
        };
        // Setting without -S/-H changes both limits, as in bash.
        let (set_soft, set_hard) = if soft || hard {
            (soft, hard)
        } else {
            (true, true)
        };
        if let Err(error) = set_limit(limit, new, set_soft, set_hard) {
            eprintln!(
                "jsh: ulimit: {}: cannot modify limit: {}",
                limit.description,
                super::os_error_text(&error)
            );
            return 1;
        }
    }
    0
}

// ============================================================
// times
// ============================================================

/// Bash's `print_timeval`: minutes, then seconds with milliseconds.
fn format_cpu_time(time: libc::timeval) -> String {
    let seconds = time.tv_sec.max(0) as u64;
    let millis = (time.tv_usec.max(0) as u64) / 1000;
    format!("{}m{}.{:03}s", seconds / 60, seconds % 60, millis)
}

fn resource_usage(who: libc::c_int) -> libc::rusage {
    // SAFETY: rusage is plain old data; all-zero is a valid value.
    let mut usage: libc::rusage = unsafe { std::mem::zeroed() };
    // SAFETY: `usage` is valid and writable for the duration of the call.
    unsafe { libc::getrusage(who, &mut usage) };
    usage
}

/// `times`: user and system CPU time of the shell, then of its finished
/// children.
pub(super) fn builtin_times(_args: &[String], _state: &mut ShellState) -> i32 {
    let shell = resource_usage(libc::RUSAGE_SELF);
    let children = resource_usage(libc::RUSAGE_CHILDREN);
    println!(
        "{} {}",
        format_cpu_time(shell.ru_utime),
        format_cpu_time(shell.ru_stime)
    );
    println!(
        "{} {}",
        format_cpu_time(children.ru_utime),
        format_cpu_time(children.ru_stime)
    );
    0
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn symbolic_modes_follow_bash() {
        // Mask 077 allows 0700.
        assert_eq!(apply_symbolic_mode("u=rwx,g=rx,o=", 0o700), Ok(0o750));
        assert_eq!(apply_symbolic_mode("g+w", 0o750), Ok(0o770));
        assert_eq!(apply_symbolic_mode("o-x,a+r", 0o755), Ok(0o754));
        assert_eq!(apply_symbolic_mode("+x", 0o700), Ok(0o711));
        assert_eq!(apply_symbolic_mode("=r", 0o777), Ok(0o444));
        assert_eq!(apply_symbolic_mode("a=", 0o777), Ok(0));
        assert!(apply_symbolic_mode("u+X", 0).is_err());
        assert!(apply_symbolic_mode("zz", 0).is_err());
        assert!(apply_symbolic_mode("u=g", 0).is_err());
    }

    #[test]
    fn symbolic_display_lists_allowed_bits() {
        assert_eq!(symbolic_umask(0o022), "u=rwx,g=rx,o=rx");
        assert_eq!(symbolic_umask(0o077), "u=rwx,g=,o=");
        assert_eq!(symbolic_umask(0o777), "u=,g=,o=");
    }

    #[test]
    fn limit_lines_use_bash_column_layout() {
        let core = LIMITS.iter().find(|l| l.option == 'c').unwrap();
        assert_eq!(
            format_limit(core, 0, true),
            "core file size              (blocks, -c) 0"
        );
        let files = LIMITS.iter().find(|l| l.option == 'n').unwrap();
        assert_eq!(
            format_limit(files, 1024, true),
            "open files                          (-n) 1024"
        );
        assert_eq!(format_limit(files, libc::RLIM_INFINITY, false), "unlimited");
    }

    #[test]
    fn cpu_times_print_minutes_and_milliseconds() {
        let time = libc::timeval {
            tv_sec: 125,
            tv_usec: 4_999,
        };
        assert_eq!(format_cpu_time(time), "2m5.004s");
    }
}
