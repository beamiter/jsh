//! Command-line parsing for the `jsh` binary.
//!
//! This deliberately stays dependency-free: shell startup is latency-sensitive,
//! and the supported surface is small enough to keep explicit and testable.

use std::ffi::OsString;
use std::io::{IsTerminal, Read};
use std::path::PathBuf;

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Input {
    /// Choose an interactive shell when stdin is a terminal, otherwise read stdin.
    Auto,
    /// Read a program from standard input.
    Stdin { args: Vec<String> },
    /// Execute a command string. The first argument after the string is `$0`.
    Command {
        command: String,
        arg0: String,
        args: Vec<String>,
    },
    /// Execute a script file. The script path is `$0`.
    Script { path: PathBuf, args: Vec<String> },
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Invocation {
    pub input: Input,
    /// Parse input without executing it (`-n`).
    pub noexec: bool,
    /// Explicitly request an interactive editor.
    pub interactive: bool,
    /// Do not load an interactive startup file.
    pub no_config: bool,
    /// Load this startup file instead of the default one.
    pub rcfile: Option<PathBuf>,
    /// Optional terminal session snapshot identifier.
    pub session_id: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ParseResult {
    Run(Invocation),
    /// Query the execution journal without starting or interpreting a shell.
    Context(Vec<String>),
    Help,
    Version,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CliError {
    message: String,
}

impl CliError {
    fn new(message: impl Into<String>) -> Self {
        Self {
            message: message.into(),
        }
    }
}

impl std::fmt::Display for CliError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.message)
    }
}

impl std::error::Error for CliError {}

pub const HELP: &str = concat!(
    "jsh — a modern Bash-inspired shell with structured data pipelines\n\n",
    "Usage:\n",
    "  jsh [OPTIONS] [SCRIPT [ARG ...]]\n",
    "  jsh [OPTIONS] -c COMMAND [NAME [ARG ...]]\n",
    "  jsh context <list|show|last-failed> [OPTIONS]\n\n",
    "Input:\n",
    "  -c, --command COMMAND  Execute COMMAND; NAME becomes $0\n",
    "  -s, --stdin            Read commands from standard input\n",
    "  -n, --noexec, --check  Parse input without executing it\n",
    "  -i, --interactive      Require an editor (incompatible with --check/input)\n\n",
    "Startup (interactive shells only):\n",
    "      --norc             Do not load a startup file\n",
    "      --rcfile FILE      Load FILE instead of the default startup file\n",
    "      --session ID       Restore and persist terminal session ID\n\n",
    "Other:\n",
    "  context ...            Query structured command execution context\n",
    "  -h, --help             Print this help\n",
    "  -V, --version          Print version information\n",
    "      --                 Stop parsing options\n\n",
    "With no SCRIPT, jsh is interactive when stdin is a terminal and otherwise\n",
    "executes commands read from stdin. Unknown options are errors.\n",
);

pub fn version() -> String {
    format!("jsh {}", env!("CARGO_PKG_VERSION"))
}

pub fn parse_env() -> Result<ParseResult, CliError> {
    let mut parsed = parse_from(std::env::args_os())?;
    if let ParseResult::Run(invocation) = &mut parsed {
        if invocation.session_id.is_none() {
            invocation.session_id = std::env::var("JSH_SESSION_ID")
                .ok()
                .filter(|id| valid_session_id(id));
        }
    }
    Ok(parsed)
}

/// Parse the process command line, dispatch it, and return the process status.
pub fn entrypoint() -> i32 {
    match parse_env() {
        Ok(ParseResult::Help) => {
            print!("{HELP}");
            0
        }
        Ok(ParseResult::Version) => {
            println!("{}", version());
            0
        }
        Ok(ParseResult::Context(args)) => crate::execution_context::run_args(&args),
        Ok(ParseResult::Run(invocation)) => run(invocation),
        Err(error) => {
            eprintln!("jsh: {error}");
            eprintln!("Try 'jsh --help' for more information.");
            2
        }
    }
}

/// Execute a previously parsed invocation.
pub fn run(invocation: Invocation) -> i32 {
    install_terminal_panic_hook();

    if invocation.noexec {
        return check_input(&invocation.input);
    }

    match invocation.input {
        Input::Command {
            command,
            arg0,
            args,
        } => crate::shell::run_command(&command, &arg0, &args),
        Input::Script { path, args } => crate::shell::run_script(&path, &args),
        Input::Stdin { args } => crate::shell::run_stdin("jsh", &args),
        Input::Auto => {
            if std::io::stdin().is_terminal() {
                let mut shell = crate::shell::Shell::new();
                shell.configure_startup(!invocation.no_config, invocation.rcfile);
                if let Some(session_id) = invocation.session_id {
                    shell.restore_session(&session_id);
                }
                shell.run()
            } else if invocation.interactive {
                eprintln!("jsh: option '-i/--interactive' requires a terminal");
                2
            } else {
                crate::shell::run_stdin("jsh", &[])
            }
        }
    }
}

fn check_input(input: &Input) -> i32 {
    let (source, label) = match input {
        Input::Command { command, .. } => (command.clone(), None),
        Input::Script { path, .. } => match std::fs::read_to_string(path) {
            Ok(content) => {
                let source = content
                    .strip_prefix("#!")
                    .and_then(|rest| rest.split_once('\n').map(|(_, body)| body.to_string()))
                    .unwrap_or(content);
                (source, Some(path.display().to_string()))
            }
            Err(error) => {
                eprintln!("jsh: {}: {error}", path.display());
                return if error.kind() == std::io::ErrorKind::NotFound {
                    127
                } else {
                    126
                };
            }
        },
        Input::Stdin { .. } | Input::Auto => {
            let mut source = String::new();
            if let Err(error) = std::io::stdin().read_to_string(&mut source) {
                eprintln!("jsh: stdin: {error}");
                return 1;
            }
            (source, Some("stdin".to_string()))
        }
    };

    let result = if crate::parser::is_incomplete(&source) {
        Err(crate::parser::parse::ParseError::Incomplete)
    } else {
        crate::parser::parse(&source)
    };
    match result {
        Ok(_) => 0,
        Err(error) => {
            match label {
                Some(label) => eprintln!("jsh: {label}: {error}"),
                None => eprintln!("jsh: {error}"),
            }
            2
        }
    }
}

fn install_terminal_panic_hook() {
    let default_hook = std::panic::take_hook();
    std::panic::set_hook(Box::new(move |info| {
        let _ = crossterm::terminal::disable_raw_mode();
        default_hook(info);
    }));
}

pub fn parse_from<I, T>(args: I) -> Result<ParseResult, CliError>
where
    I: IntoIterator<Item = T>,
    T: Into<OsString>,
{
    let args: Vec<OsString> = args.into_iter().map(Into::into).collect();
    let program_name = args
        .first()
        .and_then(|p| PathBuf::from(p).file_name().map(|s| s.to_owned()))
        .and_then(|s| s.into_string().ok())
        .filter(|s| !s.is_empty())
        .unwrap_or_else(|| "jsh".to_string());

    // `context` is a process-level query action, not a script path. Keep the
    // recognition deliberately before ordinary option/script parsing so the
    // shell never attempts to open a file literally named `context`.
    if args.get(1).and_then(|arg| arg.to_str()) == Some("context") {
        return Ok(ParseResult::Context(utf8_args(&args[2..])?));
    }

    let mut noexec = false;
    let mut interactive = false;
    let mut no_config = false;
    let mut rcfile = None;
    let mut session_id = None;
    let mut index = 1usize;
    let mut options = true;

    while index < args.len() {
        let raw = &args[index];
        let text = raw.to_str();

        if options {
            match text {
                Some("--") => {
                    options = false;
                    index += 1;
                    continue;
                }
                Some("-h" | "--help") => return Ok(ParseResult::Help),
                Some("-V" | "--version") => return Ok(ParseResult::Version),
                Some("-n" | "--noexec" | "--check") => {
                    noexec = true;
                    index += 1;
                    continue;
                }
                Some("-i" | "--interactive") => {
                    interactive = true;
                    index += 1;
                    continue;
                }
                Some("--norc") => {
                    no_config = true;
                    index += 1;
                    continue;
                }
                Some("--rcfile") => {
                    let value = args
                        .get(index + 1)
                        .ok_or_else(|| CliError::new("option '--rcfile' requires a file"))?;
                    rcfile = Some(PathBuf::from(value));
                    index += 2;
                    continue;
                }
                Some("--session") => {
                    let value =
                        required_utf8(&args, index + 1, "option '--session' requires an ID")?;
                    if !valid_session_id(&value) {
                        return Err(CliError::new(
                            "session ID must be 1-128 ASCII letters, digits, '-' or '_'",
                        ));
                    }
                    session_id = Some(value);
                    index += 2;
                    continue;
                }
                Some("-c" | "--command") => {
                    let command = required_utf8(
                        &args,
                        index + 1,
                        "option '-c/--command' requires a command string",
                    )?;
                    let trailing = utf8_args(&args[index + 2..])?;
                    let (arg0, positional) = match trailing.split_first() {
                        Some((arg0, rest)) => (arg0.clone(), rest.to_vec()),
                        None => (program_name.clone(), Vec::new()),
                    };
                    return finish(
                        Input::Command {
                            command,
                            arg0,
                            args: positional,
                        },
                        noexec,
                        interactive,
                        no_config,
                        rcfile,
                        session_id,
                    );
                }
                Some("-s" | "--stdin") => {
                    let mut trailing = &args[index + 1..];
                    if trailing.first().and_then(|s| s.to_str()) == Some("--") {
                        trailing = &trailing[1..];
                    }
                    return finish(
                        Input::Stdin {
                            args: utf8_args(trailing)?,
                        },
                        noexec,
                        interactive,
                        no_config,
                        rcfile,
                        session_id,
                    );
                }
                Some("-") => {
                    return finish(
                        Input::Stdin {
                            args: utf8_args(&args[index + 1..])?,
                        },
                        noexec,
                        interactive,
                        no_config,
                        rcfile,
                        session_id,
                    );
                }
                Some(option) if option.starts_with('-') => {
                    return Err(CliError::new(format!("unknown option '{option}'")));
                }
                _ => {}
            }
        }

        if text == Some("-") {
            return finish(
                Input::Stdin {
                    args: utf8_args(&args[index + 1..])?,
                },
                noexec,
                interactive,
                no_config,
                rcfile,
                session_id,
            );
        }

        return finish(
            Input::Script {
                path: PathBuf::from(raw),
                args: utf8_args(&args[index + 1..])?,
            },
            noexec,
            interactive,
            no_config,
            rcfile,
            session_id,
        );
    }

    finish(
        if noexec {
            Input::Stdin { args: Vec::new() }
        } else {
            Input::Auto
        },
        noexec,
        interactive,
        no_config,
        rcfile,
        session_id,
    )
}

fn finish(
    input: Input,
    noexec: bool,
    interactive: bool,
    no_config: bool,
    rcfile: Option<PathBuf>,
    session_id: Option<String>,
) -> Result<ParseResult, CliError> {
    if no_config && rcfile.is_some() {
        return Err(CliError::new("options '--norc' and '--rcfile' conflict"));
    }
    if interactive && noexec {
        return Err(CliError::new(
            "options '-i/--interactive' and '-n/--noexec' conflict",
        ));
    }
    if interactive && !matches!(&input, Input::Auto) {
        return Err(CliError::new(
            "option '-i/--interactive' cannot be combined with an explicit input",
        ));
    }
    Ok(ParseResult::Run(Invocation {
        input,
        noexec,
        interactive,
        no_config,
        rcfile,
        session_id,
    }))
}

fn required_utf8(args: &[OsString], index: usize, missing: &str) -> Result<String, CliError> {
    let value = args.get(index).ok_or_else(|| CliError::new(missing))?;
    value
        .clone()
        .into_string()
        .map_err(|_| CliError::new("command-line argument is not valid UTF-8"))
}

fn utf8_args(args: &[OsString]) -> Result<Vec<String>, CliError> {
    args.iter()
        .cloned()
        .map(|arg| {
            arg.into_string()
                .map_err(|_| CliError::new("command-line argument is not valid UTF-8"))
        })
        .collect()
}

fn valid_session_id(id: &str) -> bool {
    !id.is_empty()
        && id.len() <= 128
        && id
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b == b'-' || b == b'_')
}

#[cfg(test)]
mod tests {
    use super::*;

    fn run(args: &[&str]) -> Invocation {
        match parse_from(args).unwrap() {
            ParseResult::Run(invocation) => invocation,
            other => panic!("expected invocation, got {other:?}"),
        }
    }

    #[test]
    fn command_assigns_arg0_and_positionals_like_bash() {
        let invocation = run(&["jsh", "-c", "echo $0 $1 $#", "worker", "one", "two"]);
        assert_eq!(
            invocation.input,
            Input::Command {
                command: "echo $0 $1 $#".into(),
                arg0: "worker".into(),
                args: vec!["one".into(), "two".into()],
            }
        );
    }

    #[test]
    fn command_defaults_arg0_to_program_name() {
        let invocation = run(&["/usr/bin/jsh", "-c", "true"]);
        assert_eq!(
            invocation.input,
            Input::Command {
                command: "true".into(),
                arg0: "jsh".into(),
                args: Vec::new(),
            }
        );
    }

    #[test]
    fn script_stops_option_parsing() {
        let invocation = run(&["jsh", "script.jsh", "--norc", "x"]);
        assert_eq!(
            invocation.input,
            Input::Script {
                path: PathBuf::from("script.jsh"),
                args: vec!["--norc".into(), "x".into()],
            }
        );
        assert!(!invocation.no_config);
    }

    #[test]
    fn double_dash_allows_dash_prefixed_script() {
        let invocation = run(&["jsh", "--", "-script.jsh", "x"]);
        assert_eq!(
            invocation.input,
            Input::Script {
                path: PathBuf::from("-script.jsh"),
                args: vec!["x".into()],
            }
        );
    }

    #[test]
    fn explicit_stdin_has_positionals() {
        let invocation = run(&["jsh", "-n", "-s", "a", "b"]);
        assert!(invocation.noexec);
        assert_eq!(
            invocation.input,
            Input::Stdin {
                args: vec!["a".into(), "b".into()]
            }
        );
    }

    #[test]
    fn validates_errors_and_conflicts() {
        assert!(parse_from(["jsh", "--wat"]).is_err());
        assert!(parse_from(["jsh", "-c"]).is_err());
        assert!(parse_from(["jsh", "--rcfile"]).is_err());
        assert!(parse_from(["jsh", "--norc", "--rcfile", "x"]).is_err());
        assert!(parse_from(["jsh", "--session", "../bad"]).is_err());
        assert!(parse_from(["jsh", "-i", "-n"]).is_err());
    }

    #[test]
    fn recognizes_immediate_actions() {
        assert_eq!(parse_from(["jsh", "--help"]).unwrap(), ParseResult::Help);
        assert_eq!(
            parse_from(["jsh", "--version"]).unwrap(),
            ParseResult::Version
        );
        assert_eq!(
            parse_from(["jsh", "context", "list", "-n", "3", "--json"]).unwrap(),
            ParseResult::Context(vec![
                "list".into(),
                "-n".into(),
                "3".into(),
                "--json".into(),
            ])
        );
    }

    #[test]
    fn context_is_an_action_but_double_dash_still_allows_a_script_named_context() {
        assert!(matches!(
            parse_from(["jsh", "context", "show", "jsh-1"]).unwrap(),
            ParseResult::Context(_)
        ));
        let invocation = run(&["jsh", "--", "context", "arg"]);
        assert_eq!(
            invocation.input,
            Input::Script {
                path: PathBuf::from("context"),
                args: vec!["arg".into()],
            }
        );
    }
}
