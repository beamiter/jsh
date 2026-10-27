//! Builtins that share their name with a common system program.
//!
//! Several structured builtins (`sort`, `date`, `split`, `last`, `zip`, ...)
//! and a few classic ones (`sum`, `uniq`, `context`) carry the name of a
//! program most systems install. Taking every invocation of such a name broke
//! ordinary shell use: `printf 'b\na\n' | sort` failed to parse JSON, `date +%Y`
//! was an unknown subcommand, and `uniq -d` silently ignored its flag. Each such
//! name is declared in `SHADOWS` with a predicate that recognises jsh's own
//! forms; every other invocation runs the program found on PATH, unchanged.
//!
//! # The rule
//!
//! A shadowing builtin runs when any of these holds:
//!
//! 1. its stdin carries typed values from an upstream jsh value stage and the
//!    builtin consumes typed input (`from-json | sort`). The system program could
//!    only ever see their JSON wire form, which is never what was meant;
//! 2. its arguments use jsh's own syntax, as judged by the entry's predicate
//!    (`date now`, `split row ,`, `sum 1 2 3`);
//! 3. no program of that name is on PATH: the builtin keeps its behavior,
//!    including its errors.
//!
//! Otherwise the shell runs the program like any external command: same argv,
//! environment, redirections, stdin, stdout, stderr and exit status. `command
//! NAME` always selects the program (it is how a user asks for "the system
//! one"); `builtin NAME` always selects the builtin.
//!
//! # Where stdin comes from
//!
//! Rule 1 needs to know what fd 0 carries, and bytes alone cannot tell: a JSON
//! file piped from `cat` is text to the system `sort`. The shell therefore
//! tracks the provenance of fd 0 as an [`InputOrigin`]. A forked pipeline stage
//! records what its upstream stage runs, judged from that stage's words
//! ([`stage_output_origin`]); a stdin redirection on the command overrides it.
//! Provenance describes data piped to the stage's own command: inside a
//! compound stage or a function body (`... | while read l; do last; done`)
//! commands see the shell's ordinary stdin ([`stage_input_origin`]).
//! Two kinds of jsh stage count as text producers. The `to-*` serializers exist
//! to turn values into text. `ls` and `ps` are the external programs outside
//! the in-process value pipeline, and a shadowing builtin downstream of them is
//! judged as if they were, so `ls -l | sort -k5n` stays a text pipeline.
//!
//! The decision is made before anything reads stdin, so the system program
//! inherits fd 0 itself: no input is buffered or replayed, and an unbounded
//! stream keeps streaming.
//!
//! # Routes
//!
//! * A single command and every forked pipeline stage decide after expansion,
//!   with exact arguments ([`runs_system_program`]).
//! * The in-process value pipeline runs builtins only. It is chosen from the
//!   words before expansion ([`value_pipeline_runs_builtins`]); when any stage
//!   would run its system program, the pipeline takes the forked route instead,
//!   where each stage decides again after expansion.
//!
//! # Names left alone
//!
//! Every builtin name was checked against programs commonly installed on Linux
//! and macOS. Those not in `SHADOWS` stay builtins on purpose:
//!
//! * `ls` and `ps` are already pipeline-only routes in the command catalog, so
//!   their bare forms are the programs.
//! * `open` outside macOS: Debian's `open` is an alias of `openvt`, and the
//!   desktop opener there is `xdg-open`.
//! * `get`: a case-insensitive macOS filesystem resolves it to LWP's `GET`, but
//!   jsh's `get` is a core value command and LWP is invoked as `GET`.
//! * `echo`, `printf`, `pwd`, `test`, `[`, `true`, `false`: Bash runs its
//!   builtin as well.
//! * `format`, `stats`, `filter`, `map`, `count`, `where`, ...: no common Unix
//!   program has the name (`where` exists only on Windows).

use crate::environment::ShellState;
use crate::parser::ast::{Command, Redirect, RedirectKind, SimpleCommand, Word, WordPart};
use crate::value::Value;
use indexmap::IndexMap;
use std::sync::atomic::{AtomicU8, Ordering};

/// What a command's fd 0 carries, as far as the shell can tell before running it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[repr(u8)]
pub enum InputOrigin {
    /// The shell's own stdin, unchanged: a terminal, `/dev/null`, or the file
    /// or pipe the shell was started with.
    Inherited = 0,
    /// Bytes from an earlier stage of the same pipeline that is not a jsh value
    /// stage (an external program, a classic builtin, a compound command), or
    /// from a stdin redirection on the command itself.
    Text = 1,
    /// Typed values from an upstream jsh value stage. Across a fork they arrive
    /// in their JSON wire form.
    Values = 2,
}

/// Provenance of this process's fd 0. It is process-wide because fd 0 is: a
/// forked pipeline stage sets it once for its whole lifetime, and the in-shell
/// `lastpipe` stage sets and restores it around its one command.
static STDIN_ORIGIN: AtomicU8 = AtomicU8::new(InputOrigin::Inherited as u8);

pub fn stdin_origin() -> InputOrigin {
    decode_origin(STDIN_ORIGIN.load(Ordering::Relaxed))
}

/// Record what fd 0 now carries. Returns the previous origin so a caller that
/// replaces fd 0 temporarily can restore it.
pub fn set_stdin_origin(origin: InputOrigin) -> InputOrigin {
    decode_origin(STDIN_ORIGIN.swap(origin as u8, Ordering::Relaxed))
}

fn decode_origin(raw: u8) -> InputOrigin {
    match raw {
        1 => InputOrigin::Text,
        2 => InputOrigin::Values,
        _ => InputOrigin::Inherited,
    }
}

/// A builtin that shares its name with a system program.
pub struct Shadow {
    pub name: &'static str,
    /// Typed input from a jsh value stage selects the builtin (rule 1).
    typed_input: bool,
    /// Whether the arguments use jsh's own syntax (rule 2). The origin lets a
    /// predicate use where stdin comes from, which `last` needs.
    native_syntax: fn(&[String], InputOrigin) -> bool,
    /// When the builtin runs, completing "This builtin runs when ..." in help;
    /// `None` when it runs only as a fallback for a missing program.
    builtin_when: Option<&'static str>,
}

impl Shadow {
    /// Whether this invocation selects the builtin whatever PATH contains.
    pub fn selects_builtin(&self, args: &[String], origin: InputOrigin) -> bool {
        (self.typed_input && origin == InputOrigin::Values) || (self.native_syntax)(args, origin)
    }

    /// The paragraph `help NAME` appends to the builtin's own documentation.
    pub fn help_note(&self) -> String {
        let name = self.name;
        let when = match self.builtin_when {
            Some(when) => format!("when {when}, or when no `{name}` is on PATH"),
            None => format!("only when no `{name}` is on PATH"),
        };
        format!(
            "Shadows the system `{name}`. This builtin runs {when}; otherwise the \
             program runs with the same arguments and input. `command {name}` \
             always runs the program; `builtin {name}` always runs this builtin."
        )
    }

    /// Machine-readable form of [`Shadow::help_note`] for `help -r`.
    pub fn to_record(&self) -> Value {
        let mut record = IndexMap::new();
        record.insert("program".to_string(), Value::String(self.name.to_string()));
        record.insert(
            "builtin_when".to_string(),
            match self.builtin_when {
                Some(when) => Value::String(when.to_string()),
                None => Value::Null,
            },
        );
        Value::Record(record)
    }
}

/// Every shadowing builtin, with the program it shadows. The module docs list
/// the names considered and deliberately left alone.
static SHADOWS: &[Shadow] = &[
    // coreutils `date`: jsh's forms are subcommands, which GNU and BSD `date`
    // would reject as an invalid date or time to set.
    Shadow {
        name: "date",
        typed_input: true,
        native_syntax: date_syntax,
        builtin_when: Some("the first argument is `now` or `format`, or its input is typed values"),
    },
    // RCS `merge FILE1 FILE2 FILE3` never takes a JSON literal.
    Shadow {
        name: "merge",
        typed_input: true,
        native_syntax: merge_syntax,
        builtin_when: Some(
            "its only argument is a JSON record or table, or its input is typed values",
        ),
    },
    // util-linux/sysvinit/BSD `last` prints login history and never reads
    // stdin, so a row count over piped text can only mean the jsh filter.
    Shadow {
        name: "last",
        typed_input: true,
        native_syntax: last_syntax,
        builtin_when: Some(
            "its input is typed values, or it reads piped or redirected text with at most \
             a row count (`seq 10 | last 3`)",
        ),
    },
    // util-linux and Perl `rename OLD NEW FILE...` have the same shape as
    // jsh's column rename, so only typed input can tell them apart.
    Shadow {
        name: "rename",
        typed_input: true,
        native_syntax: no_native_syntax,
        builtin_when: Some("its input is typed values (`from-json | rename old new`)"),
    },
    // coreutils `sort`: every jsh flag (`-r`) is also a coreutils flag, so
    // only typed input selects the value sort.
    Shadow {
        name: "sort",
        typed_input: true,
        native_syntax: no_native_syntax,
        builtin_when: Some("its input is typed values from a jsh value stage (`from-json | sort`)"),
    },
    // coreutils `split [OPTION]... [FILE [PREFIX]]` would read a file named
    // `row` or `column`.
    Shadow {
        name: "split",
        typed_input: true,
        native_syntax: split_syntax,
        builtin_when: Some("the first argument is `row` or `column`, or its input is typed values"),
    },
    // Info-ZIP `zip` takes an archive name, never a JSON list.
    Shadow {
        name: "zip",
        typed_input: true,
        native_syntax: zip_syntax,
        builtin_when: Some(
            "its only argument is a JSON list (`zip '[1,2]'`), or its input is typed values",
        ),
    },
    // HTTPie's `http [METHOD] URL [ITEM...]`. jsh's lowercase method words are
    // its own syntax; HTTPie's conventional `GET`, a bare URL and every option
    // go to HTTPie. Without the `ai` feature the builtin cannot make requests,
    // so an installed HTTPie always wins.
    Shadow {
        name: "http",
        typed_input: cfg!(feature = "ai"),
        native_syntax: http_syntax,
        builtin_when: if cfg!(feature = "ai") {
            Some(
                "the first argument is `get`, `post`, `put` or `delete`, or its input is \
                 typed values",
            )
        } else {
            None
        },
    },
    // ConTeXt's `context [OPTION]... FILE` (TeX Live, MacTeX).
    Shadow {
        name: "context",
        typed_input: false,
        native_syntax: context_syntax,
        builtin_when: Some(
            "the first argument is `list`, `show`, `last-failed`, `summary` or `help`",
        ),
    },
    // coreutils `sum [OPTION]... [FILE]...` checksums files or stdin; jsh's
    // `sum` adds its numeric arguments.
    Shadow {
        name: "sum",
        typed_input: false,
        native_syntax: sum_syntax,
        builtin_when: Some("every argument is a number (`sum 1 2 3`)"),
    },
    // coreutils `uniq`: the builtin implements only `-c`, with a different
    // count width, so it is kept as a line-based fallback.
    Shadow {
        name: "uniq",
        typed_input: false,
        native_syntax: no_native_syntax,
        builtin_when: None,
    },
];

/// macOS `open` launches documents, folders and URLs. Elsewhere `open` is not
/// a desktop opener (Debian installs it as an alias of `openvt`), so jsh's
/// file reader keeps the name outright.
#[cfg(target_os = "macos")]
static PLATFORM_SHADOWS: &[Shadow] = &[Shadow {
    name: "open",
    typed_input: false,
    native_syntax: open_syntax,
    builtin_when: Some("it has exactly one operand that is not an option, a directory or a URL"),
}];
#[cfg(not(target_os = "macos"))]
static PLATFORM_SHADOWS: &[Shadow] = &[];

/// Every shadowing builtin known to this build.
pub fn entries() -> impl Iterator<Item = &'static Shadow> {
    SHADOWS.iter().chain(PLATFORM_SHADOWS.iter())
}

pub fn get(name: &str) -> Option<&'static Shadow> {
    entries().find(|shadow| shadow.name == name)
}

/// The program a shadowing builtin yields to, if one is installed. Looked up
/// in the process PATH, the same search `execvp` will perform.
pub fn system_program(name: &str) -> Option<String> {
    get(name)?;
    crate::builtins::find_in_path(name)
}

/// After expansion: whether `name args...` should run the system program
/// rather than the builtin. False for every name that does not shadow one.
pub fn runs_system_program(name: &str, args: &[String], redirects: &[Redirect]) -> bool {
    get(name).is_some_and(|shadow| {
        let origin = command_origin(redirects, stdin_origin());
        !shadow.selects_builtin(args, origin) && system_program(name).is_some()
    })
}

/// Whether the in-process value pipeline may run every stage of `cmds` as a
/// builtin. Each caller has already checked that every stage is a value-aware
/// simple command; this only rules out stages that would run a program.
pub fn value_pipeline_runs_builtins(cmds: &[Command], state: &ShellState) -> bool {
    let mut origin = stdin_origin();
    for cmd in cmds {
        if let Command::Simple(simple) = cmd {
            if stage_runs_system_program(simple, origin, state) {
                return false;
            }
        }
        origin = stage_output_origin(cmd, origin, state);
    }
    true
}

/// The origin a forked (or `lastpipe`) stage records for its own fd 0, given
/// what its upstream stage produces.
///
/// Provenance describes data piped to the stage's own command, alias
/// expansion included. A compound command or a function call runs shell code
/// whose commands read fd 0 for their own reasons: in `... | while read l; do
/// last; done` the bare `last` is the login history, not a row filter over the
/// loop's input, so such stages see the shell's ordinary stdin.
pub fn stage_input_origin(cmd: &Command, upstream: InputOrigin, state: &ShellState) -> InputOrigin {
    let runs_shell_code = match cmd {
        Command::Simple(simple) => literal_head(simple).is_some_and(|name| {
            state.functions.contains_key(name) || state.user_typed_fns.contains_key(name)
        }),
        _ => true,
    };
    if runs_shell_code {
        InputOrigin::Inherited
    } else {
        upstream
    }
}

/// What a pipeline stage hands to the next one, judged from its words before
/// expansion. `origin` is what the stage itself reads.
///
/// Only a value builtin that runs as a builtin produces values: a function or
/// alias of the same name runs instead of it, `ls`/`ps` are the external
/// programs outside the in-process route, and a shadowing builtin may run its
/// system program. The `to-*` serializers are value builtins whose whole job
/// is to turn values into text (JSON, CSV, YAML, TOML, XML, a rendered table),
/// so what follows them reads text exactly as if it came from a file.
pub fn stage_output_origin(cmd: &Command, origin: InputOrigin, state: &ShellState) -> InputOrigin {
    let Command::Simple(simple) = cmd else {
        return InputOrigin::Text;
    };
    let Some(name) = literal_head(simple) else {
        return InputOrigin::Text;
    };
    if state.functions.contains_key(name)
        || state.aliases.contains_key(name)
        || state.user_typed_fns.contains_key(name)
        || !crate::value_builtins::is_value_aware(name)
        || name.starts_with("to-")
        || stage_runs_system_program(simple, origin, state)
    {
        return InputOrigin::Text;
    }
    InputOrigin::Values
}

/// Before expansion: whether this stage runs a shadowing builtin's program.
fn stage_runs_system_program(
    simple: &SimpleCommand,
    origin: InputOrigin,
    state: &ShellState,
) -> bool {
    let Some(name) = literal_head(simple) else {
        return false;
    };
    get(name).is_some_and(|shadow| {
        let origin = command_origin(&simple.redirects, origin);
        let args = static_args(&simple.words[1..], state);
        !shadow.selects_builtin(&args, origin) && system_program(name).is_some()
    })
}

/// The command word, when it is a bare literal. Like the value pipeline's own
/// gate, anything that needs expansion to name the command is not judged.
fn literal_head(simple: &SimpleCommand) -> Option<&str> {
    match simple.words.first()?.as_slice() {
        [WordPart::Literal(name)] => Some(name.as_str()),
        _ => None,
    }
}

/// A redirection of fd 0 on the command itself replaces whatever the pipeline
/// connected, so the command reads text from a file, here-doc or descriptor.
fn command_origin(redirects: &[Redirect], origin: InputOrigin) -> InputOrigin {
    let replaces_stdin = redirects.iter().any(|redirect| match redirect.kind {
        RedirectKind::Input
        | RedirectKind::HereString
        | RedirectKind::HereDoc
        | RedirectKind::DupInput => redirect.fd.unwrap_or(0) == 0,
        _ => redirect.fd == Some(0),
    });
    if replaces_stdin {
        InputOrigin::Text
    } else {
        origin
    }
}

/// Stands in for an argument that still needs expansion. A real argument can
/// never contain NUL (argv entries are C strings), so this matches no jsh
/// syntax: every predicate treats a word it cannot see yet as not being jsh's
/// own form. Such a stage takes the forked route, which decides again after
/// expansion, so a wrong guess costs a fork rather than correctness.
const UNEXPANDED: &str = "\0";

/// Arguments as far as they are known before expansion. Literal and quoted
/// text is exact, and so is an ordinary set scalar variable (reading one has
/// no side effects; field splitting may still change the argv, which again
/// only makes a guess wrong). Anything else is [`UNEXPANDED`]: expanding it
/// here would run command substitutions and `${x:=y}` assignments twice.
fn static_args(words: &[Word], state: &ShellState) -> Vec<String> {
    words
        .iter()
        .map(|word| static_text(word, state).unwrap_or_else(|| UNEXPANDED.to_string()))
        .collect()
}

fn static_text(word: &[WordPart], state: &ShellState) -> Option<String> {
    let mut text = String::new();
    for part in word {
        match part {
            WordPart::Literal(s) | WordPart::SingleQuoted(s) => text.push_str(s),
            WordPart::DoubleQuoted(parts) => text.push_str(&static_text(parts, state)?),
            WordPart::Variable(name) if is_plain_scalar(name, state) => {
                text.push_str(state.get_var(name)?)
            }
            _ => return None,
        }
    }
    Some(text)
}

/// A set shell variable named by a bare identifier: not a parameter
/// operator, positional or special parameter, array, or typed `let` value.
fn is_plain_scalar(name: &str, state: &ShellState) -> bool {
    let mut chars = name.chars();
    chars
        .next()
        .is_some_and(|c| c.is_ascii_alphabetic() || c == '_')
        && chars.all(|c| c.is_ascii_alphanumeric() || c == '_')
        && !state.is_array(name)
        && !state.let_vars.contains_key(name)
        && state.get_var(name).is_some()
}

// ---------------------------------------------------------------------------
// Native-syntax predicates
// ---------------------------------------------------------------------------

fn no_native_syntax(_args: &[String], _origin: InputOrigin) -> bool {
    false
}

fn first_arg(args: &[String]) -> Option<&str> {
    args.first().map(String::as_str)
}

fn date_syntax(args: &[String], _origin: InputOrigin) -> bool {
    matches!(first_arg(args), Some("now" | "format"))
}

fn split_syntax(args: &[String], _origin: InputOrigin) -> bool {
    matches!(first_arg(args), Some("row" | "column"))
}

fn last_syntax(args: &[String], origin: InputOrigin) -> bool {
    // Only text this pipeline or a redirection feeds in: the shell's own stdin
    // may be a terminal or `/dev/null` under cron, where `last` means logins.
    origin == InputOrigin::Text
        && match args {
            [] => true,
            [count] => !count.is_empty() && count.bytes().all(|b| b.is_ascii_digit()),
            _ => false,
        }
}

fn zip_syntax(args: &[String], _origin: InputOrigin) -> bool {
    matches!(args, [list] if json_is(list, |value| value.is_array()))
}

fn merge_syntax(args: &[String], _origin: InputOrigin) -> bool {
    matches!(args, [other] if json_is(other, |value| value.is_object() || value.is_array()))
}

/// Only text that starts like a JSON container is parsed, so an ordinary
/// argument such as a long file name is rejected without building a value.
fn json_is(text: &str, accept: fn(&serde_json::Value) -> bool) -> bool {
    matches!(text.trim_start().as_bytes().first(), Some(b'[' | b'{'))
        && serde_json::from_str::<serde_json::Value>(text).is_ok_and(|value| accept(&value))
}

fn sum_syntax(args: &[String], _origin: InputOrigin) -> bool {
    // Decimal notation only: `inf`, `nan` and `infinity` parse as floats but
    // are just as plausibly file names for the coreutils checksum.
    !args.is_empty()
        && args.iter().all(|arg| {
            arg.bytes().any(|b| b.is_ascii_digit())
                && arg
                    .bytes()
                    .all(|b| b.is_ascii_digit() || matches!(b, b'+' | b'-' | b'.' | b'e' | b'E'))
                && arg.parse::<f64>().is_ok()
        })
}

fn context_syntax(args: &[String], _origin: InputOrigin) -> bool {
    matches!(
        first_arg(args),
        Some("list" | "show" | "last-failed" | "summary" | "help" | "-h" | "--help")
    )
}

fn http_syntax(args: &[String], _origin: InputOrigin) -> bool {
    cfg!(feature = "ai") && matches!(first_arg(args), Some("get" | "post" | "put" | "delete"))
}

/// The one form jsh's `open` serves: a single operand naming something other
/// than a directory or URL. A word not yet expanded counts as a file, which
/// keeps `open "$file" | where ...` on the in-process route.
fn open_syntax(args: &[String], _origin: InputOrigin) -> bool {
    match args {
        [operand] => {
            !operand.starts_with('-')
                && !std::path::Path::new(operand).is_dir()
                && !has_url_scheme(operand)
        }
        _ => false,
    }
}

fn has_url_scheme(text: &str) -> bool {
    text.split_once("://").is_some_and(|(scheme, _)| {
        scheme.starts_with(|c: char| c.is_ascii_alphabetic())
            && scheme
                .chars()
                .all(|c| c.is_ascii_alphanumeric() || matches!(c, '+' | '-' | '.'))
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn args(list: &[&str]) -> Vec<String> {
        list.iter().map(|arg| arg.to_string()).collect()
    }

    fn selects(name: &str, list: &[&str], origin: InputOrigin) -> bool {
        get(name).unwrap().selects_builtin(&args(list), origin)
    }

    #[test]
    fn every_shadow_is_a_dispatchable_builtin_with_help() {
        for shadow in entries() {
            assert!(
                crate::command_catalog::is_builtin(shadow.name),
                "{}",
                shadow.name
            );
            let note = shadow.help_note();
            assert!(
                note.contains(&format!("`command {}`", shadow.name)),
                "{note}"
            );
            if shadow.typed_input {
                assert!(
                    crate::value_builtins::VALUE_BUILTINS.contains_key(shadow.name),
                    "{} cannot receive typed input",
                    shadow.name
                );
            }
        }
        assert!(get("ls").is_none() && get("where").is_none() && get("cd").is_none());
    }

    #[test]
    fn typed_input_selects_value_builtins_only() {
        for name in ["sort", "date", "split", "last", "zip", "rename", "merge"] {
            assert!(selects(name, &["-n", "x"], InputOrigin::Values), "{name}");
            assert!(!selects(name, &["-n", "x"], InputOrigin::Text), "{name}");
            assert!(
                !selects(name, &["-n", "x"], InputOrigin::Inherited),
                "{name}"
            );
        }
        for name in ["sum", "uniq", "context"] {
            assert!(!selects(name, &["-x"], InputOrigin::Values), "{name}");
        }
        assert_eq!(
            selects("http", &["GET", "u"], InputOrigin::Values),
            cfg!(feature = "ai")
        );
    }

    #[test]
    fn native_syntax_recognises_only_jsh_forms() {
        use InputOrigin::*;
        assert!(selects("date", &["now"], Inherited));
        assert!(selects("date", &["format", "%Y"], Inherited));
        assert!(!selects("date", &[], Inherited));
        assert!(!selects("date", &["+%Y"], Inherited));
        assert!(!selects("date", &["-u", "now"], Inherited));

        assert!(selects("split", &["row", ","], Text));
        assert!(selects("split", &["column", ",", "a"], Inherited));
        assert!(!selects("split", &["-l", "1", "f"], Text));

        assert!(selects("zip", &["[10,20]"], Text));
        assert!(!selects("zip", &["{\"a\":1}"], Text));
        assert!(!selects("zip", &["[1]", "extra"], Text));
        assert!(!selects("zip", &["-r", "out.zip", "dir"], Inherited));

        assert!(selects("merge", &["{\"b\":1}"], Inherited));
        assert!(selects("merge", &["[{\"b\":1}]"], Inherited));
        assert!(!selects("merge", &["mine", "base", "theirs"], Inherited));

        assert!(selects("sum", &["1", "2.5", "-3", "1e3"], Inherited));
        assert!(!selects("sum", &[], Text));
        assert!(!selects("sum", &["file"], Inherited));
        assert!(!selects("sum", &["1", "file"], Inherited));
        assert!(!selects("sum", &["inf"], Inherited));
        assert!(!selects("sum", &["-r"], Inherited));

        assert!(selects("context", &["last-failed"], Inherited));
        assert!(!selects("context", &[], Inherited));
        assert!(!selects("context", &["paper.tex"], Inherited));

        assert!(!selects("sort", &[], Text));
        assert!(!selects("rename", &["a", "b"], Text));
        assert!(!selects("uniq", &["-c"], Text));
    }

    #[test]
    fn last_counts_rows_only_for_text_this_pipeline_feeds_in() {
        use InputOrigin::*;
        assert!(selects("last", &[], Text));
        assert!(selects("last", &["3"], Text));
        assert!(!selects("last", &["-n", "1"], Text));
        assert!(!selects("last", &["root"], Text));
        // The shell's own stdin is often a terminal or /dev/null; bare `last`
        // there is the login history.
        assert!(!selects("last", &[], Inherited));
        assert!(!selects("last", &["3"], Inherited));
    }

    #[test]
    fn http_methods_are_native_only_with_a_client() {
        assert_eq!(
            selects("http", &["get", "http://x"], InputOrigin::Inherited),
            cfg!(feature = "ai")
        );
        assert!(!selects("http", &["GET", "x"], InputOrigin::Inherited));
        assert!(!selects("http", &["example.org"], InputOrigin::Inherited));
        assert_eq!(
            get("http").unwrap().builtin_when.is_some(),
            cfg!(feature = "ai")
        );
    }

    #[test]
    fn open_serves_one_file_operand() {
        let temp = tempfile::tempdir().unwrap();
        let file = temp.path().join("data.json");
        std::fs::write(&file, "[]").unwrap();
        let open = |list: &[&str]| open_syntax(&args(list), InputOrigin::Inherited);
        assert!(open(&[file.to_str().unwrap()]));
        assert!(open(&["missing.json"]));
        assert!(open(&[UNEXPANDED]));
        assert!(!open(&[temp.path().to_str().unwrap()]));
        assert!(!open(&["https://example.org"]));
        assert!(!open(&["-a", "Safari"]));
        assert!(!open(&[]));
        assert!(!open(&["a.json", "b.json"]));
        assert!(has_url_scheme("x-man-page://ls"));
        assert!(!has_url_scheme("dir/://x"));
    }

    #[test]
    fn unexpanded_words_never_look_like_jsh_syntax() {
        let mut state = ShellState::new(false);
        state.set_var("sub", "now");
        let words =
            crate::parser::parse("date \"$unset_x\" 'x' \"lit\" $sub \"$(echo now)\" ${sub:-y}")
                .unwrap();
        let Command::Simple(simple) = &words[0].list.first.commands[0] else {
            panic!("simple command");
        };
        assert_eq!(
            static_args(&simple.words[1..], &state),
            args(&[UNEXPANDED, "x", "lit", "now", UNEXPANDED, UNEXPANDED])
        );
        for shadow in entries().filter(|shadow| shadow.name != "open") {
            assert!(
                !(shadow.native_syntax)(&args(&[UNEXPANDED]), InputOrigin::Text),
                "{}",
                shadow.name
            );
        }
    }

    #[test]
    fn stdin_redirections_make_the_command_read_text() {
        let parsed = crate::parser::parse("sort < f; sort 0<&3; sort <<< x; sort 2>&1").unwrap();
        let origins: Vec<InputOrigin> = parsed
            .iter()
            .map(|complete| match &complete.list.first.commands[0] {
                Command::Simple(simple) => command_origin(&simple.redirects, InputOrigin::Values),
                _ => unreachable!(),
            })
            .collect();
        use InputOrigin::*;
        assert_eq!(origins, [Text, Text, Text, Values]);
    }

    fn stages(source: &str) -> Vec<Command> {
        crate::parser::parse(source).unwrap()[0]
            .list
            .first
            .commands
            .clone()
    }

    #[test]
    fn only_value_builtins_running_as_builtins_hand_on_values() {
        use InputOrigin::*;
        let mut state = ShellState::new(false);
        let output = |source: &str, origin: InputOrigin, state: &ShellState| {
            stage_output_origin(&stages(source)[0], origin, state)
        };
        assert_eq!(output("from-json", Inherited, &state), Values);
        assert_eq!(output("where a > 1", Text, &state), Values);
        assert_eq!(output("to-json", Values, &state), Text);
        assert_eq!(output("ls", Inherited, &state), Text);
        assert_eq!(output("cat", Values, &state), Text);
        assert_eq!(output("{ from-json; }", Inherited, &state), Text);
        assert_eq!(output("\"$cmd\"", Values, &state), Text);
        // A shadowing builtin hands on values exactly when it runs as one.
        assert_eq!(output("sort", Values, &state), Values);
        let sort_without_values = if system_program("sort").is_some() {
            Text
        } else {
            Values
        };
        assert_eq!(output("sort", Text, &state), sort_without_values);
        assert_eq!(output("date now", Inherited, &state), Values);
        // A function or alias runs instead of the builtin of the same name.
        state.aliases.insert("from-json".into(), "cat".into());
        assert_eq!(output("from-json", Inherited, &state), Text);
    }

    #[test]
    fn provenance_reaches_only_the_stage_command_itself() {
        use InputOrigin::*;
        let mut state = ShellState::new(false);
        state.aliases.insert("rsort".into(), "sort -r".into());
        state.functions.insert(
            "wrapped".into(),
            match stages("f() { sort; }").remove(0) {
                Command::FunctionDef { body, .. } => *body,
                _ => unreachable!("function definition"),
            },
        );
        let input = |source: &str| stage_input_origin(&stages(source)[0], Values, &state);
        assert_eq!(input("sort -r"), Values);
        assert_eq!(input("rsort"), Values);
        assert_eq!(input("X=1 sort 2>/dev/null"), Values);
        assert_eq!(input("{ sort; }"), Inherited);
        assert_eq!(input("while read l; do last; done"), Inherited);
        assert_eq!(input("wrapped"), Inherited);
    }

    #[test]
    fn in_process_route_requires_every_stage_to_run_a_builtin() {
        // Assumes this test process's fd 0 provenance is the default; no unit
        // test changes it.
        let mut state = ShellState::new(false);
        state.set_var("sub", "now");
        let in_process = |source: &str| value_pipeline_runs_builtins(&stages(source), &state);
        assert!(in_process("from-json | sort | last 2"));
        assert!(in_process("date now | split row -"));
        assert!(in_process("date $sub | str length"));
        assert!(in_process("range 1..3 | to-json | last 2"));
        assert!(in_process("ls | get name | sort"));
        if system_program("sort").is_some() {
            assert!(!in_process("sort | from-json"));
            assert!(!in_process("ls | sort"));
            assert!(!in_process("range 1..3 | to-json | sort"));
        }
        if system_program("date").is_some() {
            assert!(!in_process("date +%s | into int"));
            assert!(!in_process("date \"$(echo now)\" | into int"));
        }
    }
}
