//! `getopts optstring name [arg ...]`, ported from bash's `sh_getopt`.
//!
//! The scan position has two parts. `OPTIND` is public: the index of the next
//! argument to examine, which scripts read (`shift $((OPTIND-1))`) and reset
//! (`OPTIND=1`, `local OPTIND`). The position *inside* a cluster such as
//! `-abc` is private to getopts, as in bash, and survives between calls only
//! while `OPTIND` still holds the value getopts itself stored. Any other value
//! means the script assigned it, which restarts the scan at that index.

use crate::environment::ShellState;

/// The part of the scan that is not visible in `OPTIND`.
#[derive(Debug, Default)]
pub struct GetoptsState {
    /// The 1-based index of the argument being scanned.
    optind: usize,
    /// Byte offset of the next option character inside a cluster, or 0 when
    /// the next call starts on a fresh argument.
    char_index: usize,
    /// What getopts last stored in `OPTIND`, to tell its own value apart from
    /// one the script assigned.
    stored_optind: Option<String>,
}

enum Outcome {
    End,
    Option(char, Option<String>),
    Invalid(char),
    MissingArgument(char),
}

pub(super) fn builtin_getopts(args: &[String], state: &mut ShellState) -> i32 {
    let [optstring, name, operands @ ..] = args else {
        eprintln!("jsh: getopts: usage: getopts optstring name [arg ...]");
        return 2;
    };
    let silent = optstring.starts_with(':');
    let optstring = optstring.strip_prefix(':').unwrap_or(optstring);
    let argv: Vec<String> = if operands.is_empty() {
        state.positional_params.clone()
    } else {
        operands.to_vec()
    };
    // Bash's OPTERR: diagnostics are on unless it holds a number that is 0.
    let opterr = state
        .get_var("OPTERR")
        .filter(|value| !value.is_empty())
        .is_none_or(|value| leading_integer(value) != 0);

    let current = state.get_var("OPTIND").map(str::to_string);
    let scan = &mut state.builtin_state.getopts;
    // `optind` is 0 only before the first call.
    if current != scan.stored_optind || scan.optind == 0 {
        // POSIX: assigning OPTIND=1 restarts the scan, and so does unsetting
        // it or storing something that is not a positive number.
        let requested = current.as_deref().map_or(0, leading_integer);
        scan.optind = usize::try_from(requested).unwrap_or(1).max(1);
        scan.char_index = 0;
    }
    let outcome = next_option(scan, &argv, optstring);
    let optind = scan.optind.to_string();
    scan.stored_optind = Some(optind.clone());

    let script = state.arg0.clone();
    state.set_var("OPTIND", &optind);
    let (value, optarg, status) = match outcome {
        Outcome::End => ("?".to_string(), None, 1),
        Outcome::Option(c, optarg) => (c.to_string(), optarg, 0),
        Outcome::Invalid(c) => {
            if !silent && opterr {
                eprintln!("{script}: illegal option -- {c}");
            }
            ("?".to_string(), silent.then(|| c.to_string()), 0)
        }
        Outcome::MissingArgument(c) => {
            if silent {
                (":".to_string(), Some(c.to_string()), 0)
            } else {
                if opterr {
                    eprintln!("{script}: option requires an argument -- {c}");
                }
                ("?".to_string(), None, 0)
            }
        }
    };
    match optarg {
        Some(optarg) => state.set_var("OPTARG", &optarg),
        None => state.unset_var("OPTARG"),
    }
    if !crate::environment::is_valid_identifier(name) {
        eprintln!("jsh: getopts: `{name}': not a valid identifier");
        return 1;
    }
    state.set_var(name, &value);
    status
}

/// One step of bash's `sh_getopt` over `argv` (whose first element is
/// argument 1).
fn next_option(scan: &mut GetoptsState, argv: &[String], optstring: &str) -> Outcome {
    // `argc` counts argv[0] in bash; OPTIND == argc means "past the end".
    let argc = argv.len() + 1;
    if scan.optind >= argc {
        scan.optind = argc;
        scan.char_index = 0;
        return Outcome::End;
    }
    let arg = &argv[scan.optind - 1];
    if scan.char_index == 0
        || scan.char_index >= arg.len()
        || !arg.is_char_boundary(scan.char_index)
    {
        if arg == "--" {
            scan.optind += 1;
            scan.char_index = 0;
            return Outcome::End;
        }
        // A non-option, including a lone `-`, ends the options without being
        // consumed.
        if !arg.starts_with('-') || arg.len() == 1 {
            scan.char_index = 0;
            return Outcome::End;
        }
        scan.char_index = 1;
    }
    let option = arg[scan.char_index..].chars().next().unwrap_or('?');
    scan.char_index += option.len_utf8();
    let rest_of_cluster = &arg[scan.char_index..];
    if rest_of_cluster.is_empty() {
        scan.optind += 1;
        scan.char_index = 0;
    }

    let takes_argument = match optstring.find(option) {
        Some(at) if option != ':' => optstring[at + option.len_utf8()..].starts_with(':'),
        _ => return Outcome::Invalid(option),
    };
    if !takes_argument {
        return Outcome::Option(option, None);
    }
    if !rest_of_cluster.is_empty() {
        // `-bvalue`: the rest of this argument is the option's argument.
        let optarg = rest_of_cluster.to_string();
        scan.optind += 1;
        scan.char_index = 0;
        return Outcome::Option(option, Some(optarg));
    }
    match argv.get(scan.optind - 1) {
        Some(optarg) => {
            scan.optind += 1;
            Outcome::Option(option, Some(optarg.clone()))
        }
        None => Outcome::MissingArgument(option),
    }
}

/// C `atoi`: optional blanks and sign, then as many digits as there are.
fn leading_integer(text: &str) -> i64 {
    let text = text.trim_start_matches([' ', '\t', '\n']);
    let (negative, digits) = match text.as_bytes().first() {
        Some(b'-') => (true, &text[1..]),
        Some(b'+') => (false, &text[1..]),
        _ => (false, text),
    };
    let value = digits
        .bytes()
        .take_while(u8::is_ascii_digit)
        .fold(0i64, |value, digit| {
            value
                .saturating_mul(10)
                .saturating_add(i64::from(digit - b'0'))
        });
    if negative {
        -value
    } else {
        value
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn scan(args: &[&str], optstring: &str) -> Vec<String> {
        let argv: Vec<String> = args.iter().map(|s| s.to_string()).collect();
        let mut state = GetoptsState {
            optind: 1,
            ..Default::default()
        };
        let mut seen = Vec::new();
        for _ in 0..20 {
            match next_option(&mut state, &argv, optstring) {
                Outcome::End => {
                    seen.push(format!("end@{}", state.optind));
                    break;
                }
                Outcome::Option(c, Some(arg)) => seen.push(format!("{c}={arg}")),
                Outcome::Option(c, None) => seen.push(c.to_string()),
                Outcome::Invalid(c) => seen.push(format!("?{c}")),
                Outcome::MissingArgument(c) => seen.push(format!(":{c}")),
            }
        }
        seen
    }

    #[test]
    fn clusters_attached_and_detached_arguments() {
        assert_eq!(
            scan(&["-ab", "val", "rest"], "ab:"),
            ["a", "b=val", "end@3"]
        );
        assert_eq!(scan(&["-bval", "x"], "b:"), ["b=val", "end@2"]);
        assert_eq!(scan(&["-a", "--", "-b"], "ab"), ["a", "end@3"]);
        assert_eq!(scan(&["-a", "-", "-b"], "ab"), ["a", "end@2"]);
        assert_eq!(scan(&["-xa"], "a"), ["?x", "a", "end@2"]);
        assert_eq!(scan(&["-a", "-b"], "ab:"), ["a", ":b", "end@3"]);
        assert_eq!(scan(&[], "a"), ["end@1"]);
    }

    #[test]
    fn atoi_reads_a_numeric_prefix() {
        assert_eq!(leading_integer(" 12abc"), 12);
        assert_eq!(leading_integer("-3"), -3);
        assert_eq!(leading_integer("x"), 0);
        assert_eq!(leading_integer(""), 0);
    }
}
