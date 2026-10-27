/// Built-in shell commands.
use crate::environment::ShellState;
use crate::parser;
use std::env;
use std::io::IsTerminal;
use std::path::Path;
use std::sync::atomic::{AtomicBool, AtomicI32, Ordering};

mod getopts;
mod limits;
mod mapfile;
mod signals;

/// Per-shell state owned by individual builtins.
pub struct BuiltinState {
    getopts: getopts::GetoptsState,
    /// Builtins turned off with `enable -n`. A disabled name resolves like any
    /// other command name (function, then `PATH`).
    disabled: std::collections::BTreeSet<String>,
    /// The `hash` table: command name to (path, lookups), in insertion order.
    hashed: indexmap::IndexMap<String, (String, u32)>,
    /// Statuses of background children `wait` has reaped, oldest first.
    reaped: std::collections::VecDeque<(i32, i32)>,
    /// When the shell started; `printf '%(...)T' -2` formats it.
    pub(crate) start_time: std::time::SystemTime,
}

impl Default for BuiltinState {
    fn default() -> Self {
        Self {
            getopts: Default::default(),
            disabled: Default::default(),
            hashed: Default::default(),
            reaped: Default::default(),
            start_time: std::time::SystemTime::now(),
        }
    }
}

/// An OS error's message without the ` (os error N)` suffix std appends,
/// so diagnostics read like bash's (`Bad file descriptor`).
fn os_error_text(error: &std::io::Error) -> String {
    let text = error.to_string();
    match text.find(" (os error") {
        Some(end) => text[..end].to_string(),
        None => text,
    }
}

/// Set by the `exit` builtin so the main loop can exit gracefully
/// (allowing session save, history save, EXIT trap to run).
pub static EXIT_REQUESTED: AtomicBool = AtomicBool::new(false);
pub static EXIT_CODE: AtomicI32 = AtomicI32::new(0);

pub fn reset_exit_request() {
    EXIT_CODE.store(0, Ordering::SeqCst);
    EXIT_REQUESTED.store(false, Ordering::SeqCst);
}

/// Low-level names accepted by the classic/fork-path builtin router.
///
/// User-facing discovery must go through [`crate::command_catalog`], which
/// also includes value-only commands and stable help metadata.
pub const BUILTIN_NAMES: &[&str] = &[
    "agent",
    "cd",
    "exit",
    "export",
    "unset",
    "echo",
    "printf",
    "pwd",
    "alias",
    "unalias",
    "type",
    "source",
    ".",
    "eval",
    "read",
    ":",
    "true",
    "false",
    "test",
    "[",
    "return",
    "caller",
    "break",
    "continue",
    "shift",
    "let",
    "set",
    "local",
    "jobs",
    "fg",
    "bg",
    "wait",
    "history",
    "context",
    "help",
    "pushd",
    "popd",
    "dirs",
    "trap",
    "command",
    "builtin",
    "hash",
    "[[",
    "declare",
    "typeset",
    "readonly",
    "z",
    "hook",
    "complete",
    "compgen",
    "disown",
    "shopt",
    "from-json",
    "to-json",
    "to-table",
    "where",
    "sort-by",
    "select",
    "bookmark",
    "workflow",
    "wf",
    "from-csv",
    "group-by",
    "unique",
    "count",
    "math",
    "exec",
    "getopts",
    "mapfile",
    "readarray",
    "umask",
    "ulimit",
    "times",
    "kill",
    "enable",
    // Stream processing commands
    "sum",
    "avg",
    "min",
    "max",
    "lines",
    "stats",
    "trim",
    "reverse",
    "upper",
    "lower",
    // Debug commands
    "debug-completion",
    "debug-trace",
    "debug-timing",
    "debug-profile",
    // Data processing commands
    "filter",
    "map",
    "dedupe",
    "shuffle",
    "uniq",
];

#[cfg(feature = "ai")]
fn builtin_agent(args: &[String], state: &mut ShellState) -> i32 {
    crate::agent::builtin_agent(args, state)
}

#[cfg(not(feature = "ai"))]
fn builtin_agent(_args: &[String], _state: &mut ShellState) -> i32 {
    eprintln!("agent: AI feature not enabled. Rebuild with --features ai");
    1
}

pub fn is_builtin(name: &str) -> bool {
    crate::command_catalog::is_builtin(name)
}

/// A builtin that `enable -n` has not turned off: what command lookup runs.
pub fn is_enabled_builtin(name: &str, state: &ShellState) -> bool {
    is_builtin(name) && !state.builtin_state.disabled.contains(name)
}

pub fn run_builtin(name: &str, args: &[String], state: &mut ShellState) -> i32 {
    match name {
        "agent" => builtin_agent(args, state),
        "cd" => builtin_cd(args, state),
        "exit" => builtin_exit(args, state),
        "export" => builtin_export(args, state),
        "unset" => builtin_unset(args, state),
        "echo" => builtin_echo(args),
        "printf" => builtin_printf(args, state),
        "pwd" => builtin_pwd(args, state),
        "alias" => builtin_alias(args, state),
        "unalias" => builtin_unalias(args, state),
        "type" => builtin_type(args, state),
        "source" | "." => builtin_source(args, state),
        "eval" => builtin_eval(args, state),
        "read" => builtin_read(args, state),
        ":" => 0,
        "true" => 0,
        "false" => 1,
        "test" | "[" => builtin_test(name, args, state),
        "set" => builtin_set(args, state),
        "local" => builtin_local(args, state),
        "return" => builtin_return(args, state),
        "caller" => crate::callstack::builtin_caller(args, state),
        "break" => builtin_loop_control("break", args, state),
        "continue" => builtin_loop_control("continue", args, state),
        "shift" => builtin_shift(args, state),
        "let" => builtin_let(args, state),
        "exec" => builtin_exec(args, state),
        "help" => builtin_help(args, state),
        "history" => builtin_history(state),
        "context" => crate::execution_context::run_args(args),
        "pushd" => builtin_pushd(args, state),
        "popd" => builtin_popd(args, state),
        "dirs" => builtin_dirs(args, state),
        "trap" => crate::traps::builtin_trap(args, state),
        "jobs" => {
            let mut print_pids = false;
            let mut running_only = false;
            let mut stopped_only = false;
            let mut args = args;
            while let Some(flag) = args.first().map(String::as_str) {
                if !flag.starts_with('-') || flag == "-" || flag == "--" {
                    break;
                }
                // Bash accepts clustered shorts (`-ps`, `-pr`) as well as
                // separate flags (`-p -s`). Reject unknown letters fail-closed.
                let mut ok = true;
                for ch in flag.chars().skip(1) {
                    match ch {
                        'p' => print_pids = true,
                        'r' => running_only = true,
                        's' => stopped_only = true,
                        _ => {
                            ok = false;
                            break;
                        }
                    }
                }
                if !ok || flag.len() < 2 {
                    break;
                }
                args = &args[1..];
            }
            if running_only && stopped_only {
                eprintln!("jsh: jobs: -r and -s are mutually exclusive");
                return 2;
            }
            if !args.is_empty() {
                eprintln!("jsh: jobs: too many arguments");
                return 2;
            }
            if print_pids && running_only {
                state.jobs.print_running_job_pids();
            } else if print_pids && stopped_only {
                state.jobs.print_stopped_job_pids();
            } else if print_pids {
                state.jobs.print_job_pids();
            } else if running_only {
                state.jobs.print_jobs_filtered(true, false);
            } else if stopped_only {
                state.jobs.print_jobs_filtered(false, true);
            } else {
                state.jobs.print_jobs();
            }
            0
        }
        "fg" => match args.first() {
            Some(spec) => match state.jobs.resolve_spec(spec) {
                Some(id) => state.jobs.continue_fg(id),
                None => {
                    eprintln!("jsh: fg: {}: no such job", spec);
                    1
                }
            },
            None => match state.jobs.get_last() {
                Some(job) => {
                    let id = job.id;
                    state.jobs.continue_fg(id)
                }
                None => {
                    eprintln!("jsh: fg: no current job");
                    1
                }
            },
        },
        "bg" => match args.first() {
            Some(spec) => match state.jobs.resolve_spec(spec) {
                Some(id) => state.jobs.continue_bg(id),
                None => {
                    eprintln!("jsh: bg: {}: no such job", spec);
                    1
                }
            },
            None => match state.jobs.get_last_stopped() {
                Some(job) => {
                    let id = job.id;
                    state.jobs.continue_bg(id)
                }
                None => {
                    eprintln!("jsh: bg: no current job");
                    1
                }
            },
        },
        "[[" => builtin_double_bracket(args, state),
        "command" => {
            // Strip the option prefix: -v/-V describe the command instead of
            // running it, -p searches the system default path.
            let mut describe: Option<char> = None;
            let mut default_path = false;
            let mut rest = args;
            while let Some(first) = rest.first() {
                let Some(flags) = first.strip_prefix('-').filter(|f| !f.is_empty()) else {
                    break;
                };
                if flags == "-" {
                    rest = &rest[1..];
                    break;
                }
                if !flags.chars().all(|c| matches!(c, 'v' | 'V' | 'p')) {
                    eprintln!("jsh: command: {first}: invalid option");
                    eprintln!("command: usage: command [-pVv] command [arg ...]");
                    return 2;
                }
                for flag in flags.chars() {
                    match flag {
                        'p' => default_path = true,
                        // Of -v and -V, the last one given wins.
                        other => describe = Some(other),
                    }
                }
                rest = &rest[1..];
            }
            if let Some(mode) = describe {
                return command_describe(rest, mode == 'V', default_path, state);
            }
            let args = rest;
            if args.is_empty() {
                return 0;
            }
            let cmd_name = &args[0];
            // `command sort` is how a user asks for the system program that a
            // structured builtin shadows; genuine shell builtins stay builtins.
            if is_enabled_builtin(cmd_name, state)
                && crate::shadowing::system_program(cmd_name).is_none()
            {
                run_builtin(cmd_name, &args[1..], state)
            } else if default_path && !cmd_name.contains('/') {
                match path_matches(cmd_name, &default_command_path())
                    .into_iter()
                    .next()
                {
                    Some(program) => {
                        let mut argv = args.to_vec();
                        argv[0] = program;
                        crate::executor::spawn_external(&argv, state)
                    }
                    None => {
                        eprintln!("jsh: {cmd_name}: command not found");
                        127
                    }
                }
            } else {
                // Hand the already-expanded argv straight to exec. Re-joining
                // and re-parsing it would destroy any argument holding
                // whitespace, quotes or newlines.
                crate::executor::spawn_external(args, state)
            }
        }
        "builtin" => {
            if args.is_empty() {
                return 0;
            }
            let cmd_name = &args[0];
            if is_enabled_builtin(cmd_name, state) {
                run_builtin(cmd_name, &args[1..], state)
            } else {
                eprintln!("jsh: builtin: {}: not a shell builtin", cmd_name);
                1
            }
        }
        "hash" => builtin_hash(args, state),
        "getopts" => getopts::builtin_getopts(args, state),
        "mapfile" | "readarray" => mapfile::builtin_mapfile(name, args, state),
        "umask" => limits::builtin_umask(args, state),
        "ulimit" => limits::builtin_ulimit(args, state),
        "times" => limits::builtin_times(args, state),
        "kill" => signals::builtin_kill(args, state),
        "enable" => builtin_enable(args, state),
        // New builtins
        "declare" | "typeset" => builtin_declare(args, state),
        "readonly" => crate::var_builtins::readonly(args, state),
        "z" => builtin_z(args, state),
        "hook" => builtin_hook(args, state),
        "complete" => builtin_complete(args, state),
        "compgen" => builtin_compgen(args, state),
        "disown" => builtin_disown(args, state),
        "wait" => builtin_wait(args, state),
        "shopt" => builtin_shopt(args, state),
        // Value-aware builtins: routed through the unified adapter at the
        // catch-all arm so single-stage AND mixed-pipeline use the same code.
        // Listed in BUILTIN_NAMES + VALUE_BUILTINS; falls through to `_`.
        "bookmark" => builtin_bookmark(args, state),
        "workflow" | "wf" => builtin_workflow(args, state),
        // Stream processing commands
        "sum" => crate::stream::builtin_sum(args),
        "avg" => crate::stream::builtin_avg(args),
        "min" => crate::stream::builtin_min(args),
        "max" => crate::stream::builtin_max(args),
        // `lines` is value-aware (Phase 6b); falls through to `_` adapter.
        "stats" => crate::stream::builtin_stats(args),
        "trim" => crate::stream::builtin_trim(args),
        // `reverse` is value-aware (Phase 5a); fall through to adapter.
        "upper" => crate::stream::builtin_upper(args),
        "lower" => crate::stream::builtin_lower(args),
        // Debug commands
        "debug-completion" => crate::debug::builtin_debug_completion(args, state),
        "debug-trace" => crate::debug::builtin_debug_trace(args),
        "debug-timing" => crate::debug::builtin_debug_timing(args),
        "debug-profile" => crate::debug::builtin_debug_profile(args),
        // Data processing commands
        "filter" => crate::data::builtin_filter(args),
        "map" => crate::data::builtin_map(args),
        // `group-by` and `select` are value-aware in Phase 5a ; fall through.
        "uniq" => crate::data::builtin_uniq(args),
        // `shuffle` is value-aware (Phase 10c) ; fall through to adapter.
        "dedupe" => crate::data::builtin_dedupe(args),
        _ => {
            // Phase 5a: fork-path adapter for value-aware builtins.
            // Reads stdin as bytes, runs the value-aware fn, writes JSON bytes
            // to stdout. Used when a value-aware builtin runs inside a forked
            // pipeline child (mixed with non-value-aware commands).
            if let Some(vfn) = crate::value_builtins::VALUE_BUILTINS.get(name) {
                return run_value_builtin_in_fork(name, *vfn, args, state);
            }
            eprintln!("jsh: {}: builtin not yet implemented", name);
            1
        }
    }
}

fn run_value_builtin_in_fork(
    name: &str,
    vfn: crate::value_builtins::ValueBuiltin,
    args: &[String],
    state: &mut ShellState,
) -> i32 {
    use crate::pipeline_data::PipelineData;
    use std::io::Write;
    const MAX_VALUE_PIPE_INPUT_BYTES: usize = 64 * 1024 * 1024;
    crate::pipeline_data::clear_stream_failure();
    let mut buf = Vec::new();
    // A source such as `range` must leave the shell's stdin to whatever
    // reads it next (see value_builtins::ignores_input).
    if !crate::value_builtins::ignores_input(name) && !std::io::stdin().is_terminal() {
        buf = match crate::io_guard::read_to_end_bounded(
            std::io::stdin().lock(),
            MAX_VALUE_PIPE_INPUT_BYTES,
        ) {
            Ok(bytes) => bytes,
            Err(error) => {
                eprintln!("jsh: value pipeline stdin: {error}");
                return 1;
            }
        };
    }
    let input = if buf.is_empty() {
        PipelineData::Empty
    } else {
        // If stdin is a JSON array (i.e. previous fork-boundary stage was also
        // value-aware), surface it as Values so per-element builtins work
        // across the fork boundary the same as they do in-process.
        let try_parse = std::str::from_utf8(&buf).ok().and_then(|s| {
            let t = s.trim();
            if t.starts_with('[') {
                serde_json::from_str::<serde_json::Value>(t).ok()
            } else {
                None
            }
        });
        match try_parse {
            Some(serde_json::Value::Array(arr)) => PipelineData::Values(
                arr.into_iter()
                    .map(crate::value::Value::from_json)
                    .collect(),
            ),
            _ => PipelineData::Bytes(buf),
        }
    };
    match vfn(input, args, state) {
        Ok(out) => {
            let mut sink: Vec<u8> = Vec::new();
            // Normalize Stream to Values for the legacy render path.
            let out = match out.interruptible() {
                PipelineData::Stream(it) => PipelineData::Values(it.collect()),
                other => other,
            };
            if let Some(message) = crate::pipeline_data::take_stream_failure() {
                eprintln!("jsh: {message}");
                return 1;
            }
            // Ctrl-C or a termination request ended the stream early; report
            // the signal rather than print a partial result.
            if let Some(status) = crate::signal::pending_status() {
                return status;
            }
            match out {
                PipelineData::Empty => {}
                PipelineData::Bytes(b) => sink.extend_from_slice(&b),
                PipelineData::Values(ref vs) => {
                    if vs.len() == 1 && !vs[0].is_record() {
                        let _ = writeln!(sink, "{}", vs[0].to_display_string());
                    } else {
                        sink.extend_from_slice(&PipelineData::Values(vs.clone()).into_bytes());
                    }
                }
                PipelineData::Stream(_) => unreachable!("normalized above"),
            }
            let _ = std::io::stdout().lock().write_all(&sink);
            0
        }
        Err(c) => {
            if let Some(message) = crate::pipeline_data::take_stream_failure() {
                eprintln!("jsh: {message}");
            }
            c
        }
    }
}

// ============================================================
// Original builtins
// ============================================================

/// Lexically resolve `.` and `..` in an absolute path, as bash's logical
/// `cd` does: `..` removes the previous component even if that component is
/// a symbolic link.
fn normalize_logical_path(path: &str) -> String {
    let mut parts: Vec<&str> = Vec::new();
    for component in path.split('/') {
        match component {
            "" | "." => {}
            ".." => {
                parts.pop();
            }
            other => parts.push(other),
        }
    }
    format!("/{}", parts.join("/"))
}

/// The shell's logical working directory: `$PWD` when it is an absolute,
/// `.`/`..`-free name for the current directory (it may go through symbolic
/// links), otherwise the physical directory.
fn logical_cwd(state: &ShellState) -> String {
    use std::os::unix::fs::MetadataExt;
    if let Some(pwd) = state.get_var("PWD") {
        let clean = pwd.starts_with('/')
            && !pwd
                .split('/')
                .any(|component| component == "." || component == "..");
        let same_directory = || match (std::fs::metadata(pwd), std::fs::metadata(".")) {
            (Ok(named), Ok(current)) => {
                named.dev() == current.dev() && named.ino() == current.ino()
            }
            _ => false,
        };
        if clean && same_directory() {
            return pwd.to_string();
        }
    }
    env::current_dir()
        .map(|dir| dir.to_string_lossy().into_owned())
        .unwrap_or_default()
}

/// The home directory a person means by `cd` alone and by `~` in `dirs`
/// output: `$HOME`, as in bash, unless `JSH_REAL_HOME` separates the home a
/// person types from the (sandboxed) `$HOME` programs write to.
fn typed_home(state: &ShellState) -> Option<String> {
    if state
        .get_var("JSH_REAL_HOME")
        .is_some_and(|real| !real.is_empty())
    {
        return Some(state.home_dir.to_string_lossy().into_owned());
    }
    state.get_var("HOME").map(str::to_string)
}

/// `set -o physical` (`set -P`) makes `cd` and `pwd` resolve symbolic links.
fn physical_by_default(state: &ShellState) -> bool {
    shell_option_enabled(state, "physical")
}

/// Change directory and return the new logical directory. The logical form
/// (`$PWD` + target, with `..` resolved lexically) is tried first, as bash
/// does without `-P`; the physical path is the fallback and the `-P` answer.
fn change_directory(target: &str, physical: bool, state: &ShellState) -> std::io::Result<String> {
    if !physical {
        let absolute = if target.starts_with('/') {
            target.to_string()
        } else {
            format!("{}/{}", logical_cwd(state), target)
        };
        let logical = normalize_logical_path(&absolute);
        if env::set_current_dir(&logical).is_ok() {
            return Ok(logical);
        }
    }
    env::set_current_dir(target)?;
    Ok(env::current_dir()?.to_string_lossy().into_owned())
}

/// Change to `target` and update `PWD`/`OLDPWD` (and run the chpwd hooks).
fn enter_directory(
    target: &str,
    physical: bool,
    state: &mut ShellState,
) -> std::io::Result<String> {
    let old = logical_cwd(state);
    let new = change_directory(target, physical, state)?;
    update_directory_vars(Some(Path::new(&old)), Path::new(&new), state);
    Ok(new)
}

fn builtin_cd(args: &[String], state: &mut ShellState) -> i32 {
    let mut physical = physical_by_default(state);
    let mut index = 0;
    while let Some(arg) = args.get(index) {
        if arg == "--" {
            index += 1;
            break;
        }
        // `-` alone is the OLDPWD operand; `-N` is a directory-stack index.
        if arg == "-" || !arg.starts_with('-') || arg[1..].bytes().all(|b| b.is_ascii_digit()) {
            break;
        }
        for flag in arg[1..].chars() {
            match flag {
                'L' => physical = false,
                'P' => physical = true,
                'e' | '@' => {}
                _ => {
                    eprintln!("jsh: cd: -{flag}: invalid option");
                    eprintln!("cd: usage: cd [-L|[-P [-e]] [-@]] [dir]");
                    return 2;
                }
            }
        }
        index += 1;
    }
    let operands = &args[index..];
    if operands.len() > 1 {
        eprintln!("jsh: cd: too many arguments");
        return 1;
    }
    let mut print = false;
    let target = match operands.first().map(String::as_str) {
        None => match typed_home(state) {
            Some(home) => home,
            None => {
                eprintln!("jsh: cd: HOME not set");
                return 1;
            }
        },
        Some("-") => match state.get_var("OLDPWD") {
            Some(dir) => {
                print = true;
                dir.to_string()
            }
            None => {
                eprintln!("jsh: cd: OLDPWD not set");
                return 1;
            }
        },
        // jsh extension: `cd +N` / `cd -N` jump to a directory-stack entry.
        Some(spec) if stack_index(spec).is_some() => {
            let entries = stack_entries(state);
            match stack_index(spec).and_then(|index| resolve_stack_index(index, entries.len())) {
                Some(position) => entries[position].clone(),
                None => {
                    eprintln!("jsh: cd: {spec}: directory stack index out of range");
                    return 1;
                }
            }
        }
        Some(dir) => dir.to_string(),
    };
    // `cd ''` stays where it is, as in bash.
    if target.is_empty() {
        return 0;
    }

    // CDPATH applies to relative names that do not start with `.` or `..`.
    let searches_cdpath = !target.starts_with('/')
        && !matches!(target.as_str(), "." | "..")
        && !target.starts_with("./")
        && !target.starts_with("../");
    if searches_cdpath {
        if let Some(cdpath) = state.get_var("CDPATH").map(str::to_string) {
            for entry in cdpath.split(':') {
                let candidate = if entry.is_empty() {
                    target.clone()
                } else {
                    format!("{}/{}", entry.trim_end_matches('/'), target)
                };
                if !Path::new(&candidate).is_dir() {
                    continue;
                }
                if let Ok(new) = enter_directory(&candidate, physical, state) {
                    // A directory found through a CDPATH entry is announced.
                    if print || !entry.is_empty() {
                        println!("{new}");
                    }
                    return 0;
                }
            }
        }
    }

    match enter_directory(&target, physical, state) {
        Ok(new) => {
            if print {
                println!("{new}");
            }
            0
        }
        Err(error) => {
            eprintln!("jsh: cd: {target}: {}", os_error_text(&error));
            1
        }
    }
}

fn update_directory_vars(
    old_dir: Option<&std::path::Path>,
    new_dir: &std::path::Path,
    state: &mut ShellState,
) {
    let new_str = new_dir.to_string_lossy().to_string();
    state.export_var("PWD", &new_str);
    if let Some(old) = old_dir {
        let old_str = old.to_string_lossy();
        state.export_var("OLDPWD", &old_str);
    }

    // Frecency describes where a person navigates, not where a script happens
    // to `cd`. Keeping this interactive-only also prevents non-interactive
    // commands from mutating dotfiles or emitting persistence warnings.
    if state.interactive {
        if let Ok(mut z_db) = crate::zjump::get_z_db().lock() {
            z_db.add(&new_dir.to_string_lossy());
        }
    }

    // chpwd hooks
    let hooks = state.hooks.chpwd.clone();
    crate::hooks::run_hooks(&hooks, state);

    // OSC 7 + OSC 1337: report CWD to terminal
    if state.interactive {
        crate::osc::report_cwd(&state.hostname);
        crate::osc::report_cwd_iterm2();
    }
}

fn builtin_exit(args: &[String], state: &ShellState) -> i32 {
    let code = match args.first() {
        Some(value) => match value.parse::<i32>() {
            Ok(code) => code,
            Err(_) => {
                eprintln!("jsh: exit: {}: numeric argument required", value);
                EXIT_CODE.store(2, Ordering::SeqCst);
                EXIT_REQUESTED.store(true, Ordering::SeqCst);
                return 2;
            }
        },
        None => state.last_exit_code,
    };

    if args.len() > 1 {
        eprintln!("jsh: exit: too many arguments");
        if !state.interactive {
            EXIT_CODE.store(1, Ordering::SeqCst);
            EXIT_REQUESTED.store(true, Ordering::SeqCst);
        }
        return 1;
    }

    EXIT_CODE.store(code, Ordering::SeqCst);
    EXIT_REQUESTED.store(true, Ordering::SeqCst);
    code
}

fn builtin_return(args: &[String], state: &mut ShellState) -> i32 {
    let code = match args.first() {
        Some(value) => match value.parse::<i32>() {
            Ok(code) => code,
            Err(_) => {
                eprintln!("jsh: return: {}: numeric argument required", value);
                if state.return_depth > 0 {
                    state.return_requested = true;
                    state.return_value = 2;
                } else {
                    eprintln!("jsh: return: can only return from a function or sourced script");
                }
                return 2;
            }
        },
        None => state.last_exit_code,
    };

    if args.len() > 1 {
        eprintln!("jsh: return: too many arguments");
        if state.return_depth > 0 {
            state.return_requested = true;
            state.return_value = 1;
        }
        return 1;
    }

    if state.return_depth == 0 {
        eprintln!("jsh: return: can only return from a function or sourced script");
        return 2;
    }

    state.return_requested = true;
    state.return_value = code;
    // The function's status travels in `return_value`. `$?` itself keeps the
    // previous command's status until the function has returned, which is
    // what a RETURN trap sees in bash.
    state.last_exit_code
}

/// `break [N]` / `continue [N]`.
///
/// N counts enclosing loops of the current function (a function never sees
/// its caller's loops); a count beyond the depth means the outermost loop.
/// As in bash, a count below 1 is reported and leaves every loop, while a
/// non-numeric or extra argument is a fatal error for a non-interactive
/// shell, because both are POSIX special builtins.
fn builtin_loop_control(name: &str, args: &[String], state: &mut ShellState) -> i32 {
    let args = match args.first().map(String::as_str) {
        Some("--") => &args[1..],
        _ => args,
    };
    let fatal = |state: &mut ShellState, status: i32| {
        if state.interactive {
            state.abort_current_program = true;
        } else {
            EXIT_CODE.store(status, Ordering::SeqCst);
            EXIT_REQUESTED.store(true, Ordering::SeqCst);
        }
        status
    };
    if args.len() > 1 {
        eprintln!("jsh: {name}: too many arguments");
        return fatal(state, 1);
    }
    // Bash's legal_number: surrounding whitespace and a sign are allowed.
    let levels = match args.first() {
        None => 1,
        Some(arg) => match arg.trim().parse::<i64>() {
            Ok(levels) => levels,
            Err(_) => {
                eprintln!("jsh: {name}: {arg}: numeric argument required");
                return fatal(state, 128);
            }
        },
    };
    if state.loop_depth == 0 {
        eprintln!("jsh: {name}: only meaningful in a `for', `while', or `until' loop");
        return 0;
    }
    if levels < 1 {
        eprintln!(
            "jsh: {name}: {}: loop count out of range",
            args.first().map_or("", |a| a.as_str())
        );
        // Bash then leaves every enclosing loop.
        state.loop_break = true;
        state.loop_unwind_levels = state.loop_depth - 1;
        return 1;
    }
    let levels = usize::try_from(levels).unwrap_or(usize::MAX);

    if name == "break" {
        state.loop_break = true;
    } else {
        state.loop_continue = true;
    }
    state.loop_unwind_levels = levels.min(state.loop_depth) - 1;
    0
}

fn builtin_export(args: &[String], state: &mut ShellState) -> i32 {
    crate::var_builtins::export(args, state)
}

fn builtin_unset(args: &[String], state: &mut ShellState) -> i32 {
    crate::var_builtins::unset(args, state)
}

fn builtin_echo(args: &[String]) -> i32 {
    use std::io::Write;
    let mut newline = true;
    let mut interpret_escapes = false;
    let mut start = 0;
    for (i, arg) in args.iter().enumerate() {
        let Some(flags) = arg.strip_prefix('-').filter(|flags| {
            !flags.is_empty() && flags.chars().all(|c| matches!(c, 'n' | 'e' | 'E'))
        }) else {
            break;
        };
        for flag in flags.chars() {
            match flag {
                'n' => newline = false,
                'e' => interpret_escapes = true,
                'E' => interpret_escapes = false,
                _ => unreachable!(),
            }
        }
        start = i + 1;
    }

    // Write to descriptor 1 itself: Rust's `Stdout` silently discards output
    // when the descriptor is closed, but `echo x >&-` must fail like bash's.
    // Anything still buffered in `Stdout` goes first to keep the order.
    let _ = std::io::stdout().flush();
    let mut output = std::io::BufWriter::new(StdoutFd);
    let result = (|| -> std::io::Result<()> {
        let mut stopped = false;
        for (i, arg) in args[start..].iter().enumerate() {
            if i > 0 {
                output.write_all(b" ")?;
            }
            if interpret_escapes {
                stopped =
                    crate::printf::escaped(&mut output, arg, crate::printf::EscapeMode::Echo)?;
                if stopped {
                    break;
                }
            } else {
                output.write_all(arg.as_bytes())?;
            }
        }
        if newline && !stopped {
            output.write_all(b"\n")?;
        }
        output.flush()
    })();
    match result {
        Ok(()) => 0,
        Err(error) => {
            match error.raw_os_error() {
                Some(code) => eprintln!(
                    "jsh: echo: write error: {}",
                    nix::errno::Errno::from_raw(code).desc()
                ),
                None => eprintln!("jsh: echo: {error}"),
            }
            1
        }
    }
}

/// Unbuffered writer on descriptor 1 that reports every error, EBADF
/// included.
struct StdoutFd;

impl std::io::Write for StdoutFd {
    fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
        // SAFETY: descriptor 1 is only borrowed for this call; an invalid
        // descriptor is reported as EBADF.
        let fd = unsafe { std::os::unix::io::BorrowedFd::borrow_raw(1) };
        nix::unistd::write(fd, buf).map_err(std::io::Error::from)
    }

    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}

fn builtin_pwd(args: &[String], state: &ShellState) -> i32 {
    let mut physical = physical_by_default(state);
    for arg in args {
        match arg.as_str() {
            "-L" => physical = false,
            "-P" => physical = true,
            "--" => break,
            other if other.starts_with('-') && other.len() > 1 => {
                eprintln!("jsh: pwd: {other}: invalid option");
                eprintln!("pwd: usage: pwd [-LP]");
                return 2;
            }
            _ => break,
        }
    }
    if physical {
        match env::current_dir() {
            Ok(dir) => {
                println!("{}", dir.display());
                0
            }
            Err(error) => {
                eprintln!(
                    "jsh: pwd: error retrieving current directory: {}",
                    os_error_text(&error)
                );
                1
            }
        }
    } else {
        let dir = logical_cwd(state);
        if dir.is_empty() {
            eprintln!("jsh: pwd: error retrieving current directory");
            return 1;
        }
        println!("{dir}");
        0
    }
}

fn builtin_alias(args: &[String], state: &mut ShellState) -> i32 {
    // `-p` only asks for the listing, which is the default anyway.
    let args: Vec<&String> = args
        .iter()
        .filter(|arg| *arg != "-p" && *arg != "--")
        .collect();
    let print = |name: &str, value: &str| {
        println!("alias {name}={}", parser::unparse::single_quote(value));
    };
    if args.is_empty() {
        let mut names: Vec<&String> = state.aliases.keys().collect();
        names.sort();
        for name in names {
            print(name, &state.aliases[name]);
        }
        return 0;
    }
    let mut status = 0;
    for arg in args {
        if let Some(eq_pos) = arg.find('=') {
            // The value is stored exactly as given; quoting in it is
            // interpreted when the alias is expanded.
            let name = &arg[..eq_pos];
            let value = &arg[eq_pos + 1..];
            state.aliases.insert(name.to_string(), value.to_string());
        } else {
            match state.aliases.get(arg.as_str()) {
                Some(value) => print(arg, value),
                None => {
                    eprintln!("jsh: alias: {}: not found", arg);
                    status = 1;
                }
            }
        }
    }
    status
}

fn builtin_unalias(args: &[String], state: &mut ShellState) -> i32 {
    if args.is_empty() {
        eprintln!("unalias: usage: unalias [-a] name [name ...]");
        return 2;
    }
    let mut status = 0;
    for name in args {
        if name == "-a" {
            state.aliases.clear();
            continue;
        }
        if name == "--" {
            continue;
        }
        if state.aliases.remove(name).is_none() {
            eprintln!("jsh: unalias: {name}: not found");
            status = 1;
        }
    }
    status
}

/// How `type` and `command -v/-V` report a name.
#[derive(Clone, Copy, PartialEq, Eq)]
enum Describe {
    /// `command -v`: the name, the alias definition, or the file's path.
    Terse,
    /// `type`, `command -V`: a sentence, with a function's definition.
    Verbose,
    /// `type -t`: one word (`alias`, `keyword`, `function`, `builtin`, `file`).
    Kind,
    /// `type -p`: a path, and only when the name would run a file.
    PathOnly,
}

#[derive(Clone, Copy)]
struct DescribeOptions {
    style: Describe,
    /// `type -a`: every match, not just the one that would run.
    all: bool,
    /// `type -f`: skip shell functions.
    skip_functions: bool,
    /// `type -P`: search `PATH` even for aliases, builtins and functions.
    force_path: bool,
    /// `command -p`: search the system default path instead of `PATH`.
    default_path: bool,
}

fn builtin_type(args: &[String], state: &mut ShellState) -> i32 {
    let mut options = DescribeOptions {
        style: Describe::Verbose,
        all: false,
        skip_functions: false,
        force_path: false,
        default_path: false,
    };
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
                't' => options.style = Describe::Kind,
                'p' => options.style = Describe::PathOnly,
                'P' => {
                    options.style = Describe::PathOnly;
                    options.force_path = true;
                }
                'a' => options.all = true,
                'f' => options.skip_functions = true,
                _ => {
                    eprintln!("jsh: type: -{flag}: invalid option");
                    eprintln!("type: usage: type [-afptP] name [name ...]");
                    return 2;
                }
            }
        }
        index += 1;
    }
    let mut status = 0;
    for name in &args[index..] {
        if !describe_command(name, options, state) {
            if options.style == Describe::Verbose {
                eprintln!("jsh: type: {name}: not found");
            }
            status = 1;
        }
    }
    status
}

/// Every executable file named `name` along `path`, in search order. A name
/// with a slash is not searched for; it is reported if it is executable.
fn path_matches(name: &str, path: &str) -> Vec<String> {
    if name.contains('/') {
        return if is_executable_file(Path::new(name)) {
            vec![name.to_string()]
        } else {
            Vec::new()
        };
    }
    let mut found: Vec<String> = Vec::new();
    for dir in path.split(':') {
        let dir = if dir.is_empty() { "." } else { dir };
        let full = format!("{dir}/{name}");
        if is_executable_file(Path::new(&full)) && !found.contains(&full) {
            found.push(full);
        }
    }
    found
}

/// The system's default command path, used by `command -p`.
fn default_command_path() -> String {
    let mut buffer = [0u8; 256];
    // SAFETY: confstr writes at most buffer.len() bytes, NUL included.
    let length = unsafe {
        nix::libc::confstr(
            nix::libc::_CS_PATH,
            buffer.as_mut_ptr().cast(),
            buffer.len(),
        )
    };
    if length > 1 && length <= buffer.len() {
        String::from_utf8_lossy(&buffer[..length - 1]).into_owned()
    } else {
        "/bin:/usr/bin".to_string()
    }
}

/// Report how `name` resolves, in the order the shell resolves a command:
/// alias, keyword, function, builtin, then files on the search path. Returns
/// whether anything was found.
fn describe_command(name: &str, options: DescribeOptions, state: &ShellState) -> bool {
    let style = options.style;
    let mut found = false;
    if !options.force_path {
        if let Some(value) = state.aliases.get(name) {
            match style {
                Describe::Terse => {
                    println!("alias {name}={}", parser::unparse::single_quote(value))
                }
                Describe::Verbose => println!("{name} is aliased to `{value}'"),
                Describe::Kind => println!("alias"),
                Describe::PathOnly => {}
            }
            if !options.all {
                return true;
            }
            found = true;
        }
        if is_shell_keyword(name) {
            match style {
                Describe::Terse => println!("{name}"),
                Describe::Verbose => println!("{name} is a shell keyword"),
                Describe::Kind => println!("keyword"),
                Describe::PathOnly => {}
            }
            if !options.all {
                return true;
            }
            found = true;
        }
        if !options.skip_functions {
            if let Some(body) = state.functions.get(name) {
                match style {
                    Describe::Terse => println!("{name}"),
                    Describe::Verbose => println!(
                        "{name} is a function\n{}",
                        parser::unparse::format_function(name, body)
                    ),
                    Describe::Kind => println!("function"),
                    Describe::PathOnly => {}
                }
                if !options.all {
                    return true;
                }
                found = true;
            }
        }
        // A builtin that shadows an installed program (`sort`, `date`, ...)
        // yields to it for ordinary use, so report the program first; `-a`
        // still lists the builtin, and `help NAME` says when it runs.
        if is_enabled_builtin(name, state)
            && (options.all || crate::shadowing::system_program(name).is_none())
        {
            match style {
                Describe::Terse => println!("{name}"),
                Describe::Verbose => println!("{name} is a shell builtin"),
                Describe::Kind => println!("builtin"),
                Describe::PathOnly => {}
            }
            if !options.all {
                return true;
            }
            found = true;
        }
    }
    let search_path = if options.default_path {
        default_command_path()
    } else {
        state.get_var("PATH").unwrap_or_default().to_string()
    };
    let mut paths = path_matches(name, &search_path);
    if !options.all {
        paths.truncate(1);
    }
    for path in paths {
        match style {
            Describe::Terse | Describe::PathOnly => println!("{path}"),
            Describe::Verbose => println!("{name} is {path}"),
            Describe::Kind => println!("file"),
        }
        found = true;
    }
    found
}

pub(crate) fn find_in_path(cmd: &str) -> Option<String> {
    // A name containing a slash is used as-is, never searched for in PATH.
    if cmd.contains('/') {
        return if is_executable_file(Path::new(cmd)) {
            Some(cmd.to_string())
        } else {
            None
        };
    }
    if let Ok(path) = env::var("PATH") {
        for dir in path.split(':') {
            let dir = if dir.is_empty() { "." } else { dir };
            let full = format!("{}/{}", dir, cmd);
            if is_executable_file(Path::new(&full)) {
                return Some(full);
            }
        }
    }
    None
}

fn is_executable_file(path: &Path) -> bool {
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        match std::fs::metadata(path) {
            Ok(md) => md.is_file() && md.permissions().mode() & 0o111 != 0,
            Err(_) => false,
        }
    }
    #[cfg(not(unix))]
    {
        path.is_file()
    }
}

fn is_shell_keyword(name: &str) -> bool {
    matches!(
        name,
        "if" | "then"
            | "else"
            | "elif"
            | "fi"
            | "for"
            | "while"
            | "until"
            | "do"
            | "done"
            | "case"
            | "esac"
            | "in"
            | "function"
            | "select"
            | "time"
            | "{"
            | "}"
            | "!"
            | "[["
            | "]]"
            | "coproc"
    )
}

/// `command -v NAME...` / `command -V NAME...`: report how each name would be
/// resolved, without running it. As in bash, the status is 0 if any name was
/// found.
fn command_describe(
    names: &[String],
    verbose: bool,
    default_path: bool,
    state: &ShellState,
) -> i32 {
    let options = DescribeOptions {
        style: if verbose {
            Describe::Verbose
        } else {
            Describe::Terse
        },
        all: false,
        skip_functions: false,
        force_path: false,
        default_path,
    };
    let mut any_found = false;
    for name in names {
        if describe_command(name, options, state) {
            any_found = true;
        } else if verbose {
            eprintln!("jsh: command: {name}: not found");
        }
    }
    if any_found {
        0
    } else {
        1
    }
}

/// Bash's POSIX special builtins, which `enable -s` lists.
const SPECIAL_BUILTINS: &[&str] = &[
    "break", ":", ".", "continue", "eval", "exec", "exit", "export", "readonly", "return", "set",
    "shift", "times", "trap", "unset",
];

/// Builtins `enable` can list and toggle: every builtin except `[[`, which is
/// a keyword (bash says so too).
fn toggleable_builtins() -> impl Iterator<Item = &'static str> {
    crate::command_catalog::builtin_names()
        .iter()
        .copied()
        .filter(|name| *name != "[[")
}

/// `enable [-anps] [name ...]`: list builtins, or turn them off (`-n`) and on.
/// A disabled builtin's name then runs a function or a file of that name.
fn builtin_enable(args: &[String], state: &mut ShellState) -> i32 {
    let mut disable = false;
    let mut all = false;
    let mut special = false;
    let mut print = false;
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
                'n' => disable = true,
                'a' => all = true,
                's' => special = true,
                'p' => print = true,
                'f' | 'd' => {
                    eprintln!("jsh: enable: -{flag}: loadable builtins are not supported");
                    return 1;
                }
                _ => {
                    eprintln!("jsh: enable: -{flag}: invalid option");
                    eprintln!("enable: usage: enable [-a] [-dnps] [-f filename] [name ...]");
                    return 2;
                }
            }
        }
        index += 1;
    }
    let names = &args[index..];
    if names.is_empty() || print {
        // -a lists everything; -n the disabled builtins; otherwise the
        // enabled ones. Each line re-creates the current setting.
        for name in toggleable_builtins() {
            if special && !SPECIAL_BUILTINS.contains(&name) {
                continue;
            }
            let disabled = state.builtin_state.disabled.contains(name);
            if !all && disabled != disable {
                continue;
            }
            println!("enable {}{name}", if disabled { "-n " } else { "" });
        }
        return 0;
    }
    let mut status = 0;
    for name in names {
        if name == "[[" || !is_builtin(name) {
            eprintln!("jsh: enable: {name}: not a shell builtin");
            status = 1;
        } else if disable {
            state.builtin_state.disabled.insert(name.clone());
        } else {
            state.builtin_state.disabled.remove(name.as_str());
        }
    }
    status
}

/// `hash [-lrt] [-p path] [-d] [name ...]`: remember, list and forget command
/// locations. jsh looks commands up on every run, so the table records what
/// was asked for (`hash -t` and `hash -l` report it) without short-cutting
/// execution.
fn builtin_hash(args: &[String], state: &mut ShellState) -> i32 {
    let mut clear = false;
    let mut delete = false;
    let mut print_paths = false;
    let mut reusable = false;
    let mut explicit_path: Option<String> = None;
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
                'r' => clear = true,
                'd' => delete = true,
                't' => print_paths = true,
                'l' => reusable = true,
                'p' => match args.get(index) {
                    Some(path) => {
                        explicit_path = Some(path.clone());
                        index += 1;
                    }
                    None => {
                        eprintln!("jsh: hash: -p: option requires an argument");
                        return 1;
                    }
                },
                _ => {
                    eprintln!("jsh: hash: -{flag}: invalid option");
                    eprintln!("hash: usage: hash [-lr] [-p pathname] [-dt] [name ...]");
                    return 2;
                }
            }
        }
    }
    if clear {
        state.builtin_state.hashed.clear();
    }
    let names = &args[index..];
    if names.is_empty() {
        if print_paths || delete {
            eprintln!(
                "jsh: hash: -{}: option requires an argument",
                if print_paths { 't' } else { 'd' }
            );
            return 1;
        }
        if clear || explicit_path.is_some() {
            return 0;
        }
        let table = &state.builtin_state.hashed;
        if table.is_empty() {
            if !reusable {
                println!("hash: hash table empty");
            }
            return 0;
        }
        if !reusable {
            println!("hits\tcommand");
        }
        for (name, (path, hits)) in table {
            if reusable {
                println!("builtin hash -p {path} {name}");
            } else {
                println!("{hits:4}\t{path}");
            }
        }
        return 0;
    }
    let mut status = 0;
    for name in names {
        if delete {
            if state.builtin_state.hashed.shift_remove(name).is_none() {
                eprintln!("jsh: hash: {name}: not found");
                status = 1;
            }
        } else if print_paths {
            // Bash counts a `-t` lookup as a use of the entry.
            match state.builtin_state.hashed.get_mut(name) {
                Some((path, hits)) => {
                    *hits += 1;
                    if names.len() > 1 {
                        println!("{name}\t{path}");
                    } else {
                        println!("{path}");
                    }
                }
                None => {
                    eprintln!("jsh: hash: {name}: not found");
                    status = 1;
                }
            }
        } else if let Some(path) = &explicit_path {
            state
                .builtin_state
                .hashed
                .insert(name.clone(), (path.clone(), 0));
        } else if name.contains('/')
            || is_enabled_builtin(name, state)
            || state.functions.contains_key(name)
        {
            // Bash hashes only commands it would look up on PATH.
        } else {
            let path = state.get_var("PATH").unwrap_or_default().to_string();
            match path_matches(name, &path).into_iter().next() {
                Some(found) => {
                    state.builtin_state.hashed.insert(name.clone(), (found, 0));
                }
                None => {
                    eprintln!("jsh: hash: {name}: not found");
                    status = 1;
                }
            }
        }
    }
    status
}

/// Drop the two lines an interactive bash prints when it is started without a
/// controlling terminal.
///
/// The helper has to be interactive ; that is the only way the startup file it
/// is sourcing will run past its own `case $- in *i*)` guard ; but its stdin is
/// a pipe, so bash announces that it cannot claim the terminal. Reporting that
/// as a warning from the sourced file would put two lines of noise under every
/// `source` of anything jsh's own parser could not read.
fn strip_job_control_notices(stderr: &str) -> String {
    stderr
        .lines()
        .filter(|line| {
            !line.contains("cannot set terminal process group")
                && !line.contains("no job control in this shell")
        })
        .collect::<Vec<_>>()
        .join("\n")
}

/// Use bash to source a script file when jsh's parser can't handle it, then
/// reload its exported environment back into jsh.
fn source_via_bash(path: &str, source_args: &[String], state: &mut ShellState) -> i32 {
    const MAX_SOURCE_HELPER_OUTPUT_BYTES: usize = 16 * 1024 * 1024;
    const MAX_SOURCE_HELPER_STDERR_BYTES: usize = 1024 * 1024;
    // Create a bash script that sources the file and outputs environment variables
    let bash_script = r#"
# A startup file decides whether anyone is listening by looking at PS1 and at
# `$-`; both have to say the same thing or the file stops at its own first
# guard. See the matching helper in config.rs.
export PS1='$ '

set -a
source -- "$1" "${@:2}"
builtin set -- "$?"
builtin set +a
builtin trap - DEBUG RETURN ERR EXIT
builtin set +o history

# Preserve arbitrary exported values without reparsing shell quoting.  NUL is
# forbidden inside an environment entry, so `NAME=VALUE\0` is unambiguous.
# Use a fixed system `env` so a sourced PATH or function cannot replace it.
# Exclude helper-owned values while retaining PS1/HISTFILE if the sourced file
# deliberately changed their sentinel values. `$1` now holds the source
# status. Build the env argv in a subshell's positional parameters so no helper
# variable can collide with an exported value from the sourced file.
builtin printf '\0JSH_ENVIRONMENT_V1\0'
(
    builtin set -- -u SHLVL -u _
    [[ ${HISTFILE-} == /dev/null ]] && builtin set -- "$@" -u HISTFILE
    [[ ${PS1-} == '$ ' ]] && builtin set -- "$@" -u PS1
    if [[ -x /usr/bin/env ]]; then
        /usr/bin/env "$@" -0
    elif [[ -x /bin/env ]]; then
        /bin/env "$@" -0
    else
        builtin exit 127
    fi
) || builtin exit "$?"
builtin printf '\0'
builtin exit "$1"
"#;

    // Execute bash script to capture the environment
    let Some(bash) = crate::io_guard::trusted_helper("bash") else {
        eprintln!("jsh: source: no trusted system Bash is available");
        return 1;
    };
    let mut command = std::process::Command::new(bash);
    let prior_environment = state.configure_command_environment(&mut command);
    command
        .arg("--norc")
        .arg("--noprofile")
        .arg("-i")
        .arg("-c")
        .arg(bash_script)
        .arg("jsh-source")
        .arg(path)
        .args(source_args)
        .env("HISTFILE", "/dev/null");
    match crate::io_guard::bounded_command_output_detached(
        &mut command,
        MAX_SOURCE_HELPER_OUTPUT_BYTES,
        MAX_SOURCE_HELPER_STDERR_BYTES,
        std::time::Duration::from_secs(300),
    ) {
        Ok(output) => {
            let stderr = String::from_utf8_lossy(&output.stderr);

            // If bash had errors, print them but continue
            let stderr = strip_job_control_notices(&stderr);
            if !stderr.is_empty() && !stderr.contains("warning") {
                eprintln!(
                    "jsh: bash source warnings: {}",
                    crate::terminal_text::escape_inline(stderr.trim(), 16 * 1024)
                );
            }

            if crate::config::import_bash_environment_frame(
                &output.stdout,
                state,
                &prior_environment,
            )
            .is_none()
            {
                eprintln!("jsh: source: bash fallback returned malformed environment framing");
                return 1;
            }

            output.status.code().unwrap_or(1)
        }
        Err(e) => {
            eprintln!("jsh: source: failed to execute bash fallback: {}", e);
            1
        }
    }
}

fn builtin_source(args: &[String], state: &mut ShellState) -> i32 {
    if args.is_empty() {
        eprintln!("jsh: source: filename argument required");
        return 1;
    }

    let filename = &args[0];
    let additional_args = &args[1..];

    // Try to find the file
    let resolved_path = if Path::new(filename).is_file() {
        // File exists at given path
        filename.to_string()
    } else if !filename.contains('/') {
        // No slashes in path, try multiple sources
        // 1. Try current directory
        if Path::new(filename).is_file() {
            filename.to_string()
        } else if let Some(found) = find_in_path(filename) {
            // 2. Try $PATH
            found
        } else {
            eprintln!("jsh: source: {}: No such file or directory", filename);
            return 1;
        }
    } else {
        // Absolute or relative path doesn't exist
        eprintln!("jsh: source: {}: No such file or directory", filename);
        return 1;
    };

    // Bash preserves `$0` while sourcing. Explicit source arguments temporarily
    // replace `$1..`; without arguments, the caller's parameters stay visible.
    let old_params = state.positional_params.clone();
    let source_params = if additional_args.is_empty() {
        old_params.clone()
    } else {
        additional_args.to_vec()
    };
    if !additional_args.is_empty() {
        state.positional_params = source_params.clone();
    }

    // Bash exposes the file being sourced as `${BASH_SOURCE[0]}`, with outer
    // frames following it. Setup scripts use it to locate their own directory,
    // so push a frame for the duration of the source. Its lines are counted
    // from the top of the file.
    let call_line = state.call_stack.lineno();
    state.push_call_frame(crate::callstack::CallFrame::sourced_file(
        &resolved_path,
        call_line,
    ));
    let saved_offset = std::mem::replace(&mut state.call_stack.line_offset, 0);

    state.return_depth += 1;
    let result = match crate::io_guard::read_regular_text(
        std::path::Path::new(&resolved_path),
        16 * 1024 * 1024,
    ) {
        Ok(content) => {
            match parser::parse(&content) {
                Ok(commands) => {
                    // Parse succeeded, execute all commands in current shell context
                    let last = crate::executor::execute_program(&commands, state);
                    // `return` exits a sourced file but must not leak into the
                    // caller's command list.
                    let last = if state.return_requested {
                        state.return_requested = false;
                        std::mem::take(&mut state.return_value)
                    } else {
                        last
                    };
                    // The RETURN trap also fires when a sourced file finishes.
                    crate::traps::run_return_trap(state);
                    last
                }
                Err(e) => {
                    eprintln!("jsh: source: {}: parse error: {}", resolved_path, e);
                    // Try bash as fallback only for complex scripts
                    source_via_bash(&resolved_path, &source_params, state)
                }
            }
        }
        Err(e) => {
            eprintln!("jsh: source: {}: {}", resolved_path, e);
            1
        }
    };
    state.return_depth -= 1;

    // Restore state
    state.positional_params = old_params;
    state.pop_call_frame();
    state.call_stack.line_offset = saved_offset;
    state.call_stack.set_lineno(call_line);

    result
}

fn builtin_eval(args: &[String], state: &mut ShellState) -> i32 {
    if args.is_empty() {
        return 0;
    }

    // Join all arguments with space, just like bash does
    let input = args.join(" ");

    // Parse and execute the input. Its first line is the line `eval` is on.
    match parser::parse(&input) {
        Ok(commands) => {
            let line = state.call_stack.lineno();
            let nested_offset = state.call_stack.nested_line_offset();
            let saved_offset = std::mem::replace(&mut state.call_stack.line_offset, nested_offset);
            let status = crate::executor::execute_program(&commands, state);
            state.call_stack.line_offset = saved_offset;
            state.call_stack.set_lineno(line);
            status
        }
        Err(e) => {
            eprintln!("jsh: eval: parse error: {}", e);
            2
        }
    }
}

/// Upper bound on the bytes a single `read` collects. Input such as
/// `read -d '' < /dev/zero` never reaches its delimiter; failing loudly
/// beats exhausting memory.
const MAX_READ_BYTES: usize = 256 * 1024 * 1024;

const READ_USAGE: &str = "read: usage: read [-ers] [-a array] [-d delim] [-i text] [-n nchars] [-N nchars] [-p prompt] [-t timeout] [-u fd] [name ...]";

/// Parsed `read` options.
struct ReadOptions {
    raw: bool,
    silent: bool,
    prompt: Option<String>,
    timeout: Option<std::time::Duration>,
    /// `-n`/`-N` character count; `exact` is `-N`, which ignores the
    /// delimiter and does no field splitting.
    count: Option<usize>,
    exact: bool,
    /// The record delimiter; `-d ''` is NUL.
    delimiter: char,
    array: Option<String>,
    fd: std::os::fd::RawFd,
}

/// `read`'s options, which may cluster (`-rn 2`) or carry their value in the
/// same word (`-d,`, `-n2`), as with bash's getopt. Returns the options and
/// the index of the first name, or the exit status for a usage error.
fn parse_read_options(args: &[String]) -> Result<(ReadOptions, usize), i32> {
    let mut options = ReadOptions {
        raw: false,
        silent: false,
        prompt: None,
        timeout: None,
        count: None,
        exact: false,
        delimiter: '\n',
        array: None,
        fd: 0,
    };
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
        for (at, flag) in flags.char_indices() {
            match flag {
                'r' => options.raw = true,
                's' => options.silent = true,
                // Readline editing (-e) and its initial text (-i) only apply
                // when reading from a terminal through readline, which jsh's
                // `read` does not use; they are accepted and ignored.
                'e' => {}
                'a' | 'd' | 'i' | 'n' | 'N' | 'p' | 't' | 'u' => {
                    let attached = &flags[at + flag.len_utf8()..];
                    let value = if !attached.is_empty() {
                        attached.to_string()
                    } else if let Some(next) = args.get(index) {
                        index += 1;
                        next.clone()
                    } else {
                        eprintln!("jsh: read: -{flag}: option requires an argument");
                        eprintln!("{READ_USAGE}");
                        return Err(2);
                    };
                    match flag {
                        'a' => options.array = Some(value),
                        'd' => options.delimiter = value.chars().next().unwrap_or('\0'),
                        'i' => {}
                        'p' => options.prompt = Some(value),
                        'n' | 'N' => match value.parse::<usize>() {
                            Ok(count) => {
                                options.count = Some(count);
                                options.exact = flag == 'N';
                            }
                            Err(_) => {
                                eprintln!("jsh: read: {value}: invalid number");
                                return Err(1);
                            }
                        },
                        't' => match parse_read_timeout(&value) {
                            Some(timeout) => options.timeout = Some(timeout),
                            None => {
                                eprintln!("jsh: read: {value}: invalid timeout specification");
                                return Err(1);
                            }
                        },
                        _ => match value.parse::<std::os::fd::RawFd>() {
                            Ok(fd) if fd >= 0 => options.fd = fd,
                            _ => {
                                eprintln!(
                                    "jsh: read: {value}: invalid file descriptor specification"
                                );
                                return Err(1);
                            }
                        },
                    }
                    break;
                }
                _ => {
                    eprintln!("jsh: read: -{flag}: invalid option");
                    eprintln!("{READ_USAGE}");
                    return Err(2);
                }
            }
        }
    }
    Ok((options, index))
}

fn builtin_read(args: &[String], state: &mut ShellState) -> i32 {
    let (options, first_name) = match parse_read_options(args) {
        Ok(parsed) => parsed,
        Err(status) => return status,
    };
    let names = &args[first_name..];
    // Names are checked before anything is read, so a typo does not swallow
    // the line the next command expected.
    if let Some(array) = &options.array {
        if !crate::environment::is_valid_identifier(array) {
            eprintln!("jsh: read: `{array}': not a valid identifier");
            return 1;
        }
    }
    if let Some(bad) = names.iter().find(|name| variable_target(name).is_none()) {
        eprintln!("jsh: read: `{bad}': not a valid identifier");
        return 1;
    }
    let fd = options.fd;
    // SAFETY: F_GETFD only queries the descriptor's flags.
    if unsafe { nix::libc::fcntl(fd, nix::libc::F_GETFD) } < 0 {
        let error = std::io::Error::last_os_error();
        eprintln!(
            "jsh: read: {fd}: invalid file descriptor: {}",
            os_error_text(&error)
        );
        return 1;
    }

    if options.timeout == Some(std::time::Duration::ZERO) {
        return poll_read_fd(fd, 0).map_or_else(|status| status, |ready| if ready { 0 } else { 1 });
    }
    let deadline = match options.timeout {
        Some(timeout) => match std::time::Instant::now().checked_add(timeout) {
            Some(deadline) => Some(deadline),
            None => {
                eprintln!("jsh: read: invalid timeout specification");
                return 1;
            }
        },
        None => None,
    };

    // The prompt and the silent mode only mean something on a terminal.
    // SAFETY: isatty only inspects the descriptor.
    let terminal = unsafe { nix::libc::isatty(fd) } == 1;
    if let (Some(prompt), true) = (&options.prompt, terminal) {
        use std::io::Write;
        eprint!("{prompt}");
        std::io::stderr().flush().ok();
    }
    let echo_guard = (options.silent && terminal)
        .then(|| EchoGuard::disable(fd))
        .flatten();
    let (input, status) = read_input(&options, deadline);
    drop(echo_guard);

    if status != 0 && status != 1 && status != READ_TIMEOUT_STATUS {
        // An error or a signal: nothing is assigned.
        return status;
    }

    if let Some(array) = &options.array {
        let ifs = if options.exact {
            String::new()
        } else {
            state.get_var("IFS").unwrap_or(" \t\n").to_string()
        };
        let mut fields = Vec::new();
        let mut rest = trim_leading_ifs_whitespace(&input, &ifs);
        while !rest.is_empty() {
            let (field, after) = take_ifs_field(rest, &ifs);
            fields.push(field);
            rest = after;
        }
        state.set_array(array, fields);
        return status;
    }
    if names.is_empty() {
        // REPLY gets the line as read: escapes removed, nothing trimmed.
        let line: String = input.iter().map(|&(c, _)| c).collect();
        state.set_var("REPLY", &line);
        return status;
    }

    // -N assigns the characters read without splitting them.
    let ifs = if options.exact {
        String::new()
    } else {
        state.get_var("IFS").unwrap_or(" \t\n").to_string()
    };
    let mut rest = trim_leading_ifs_whitespace(&input, &ifs);
    for (index, name) in names.iter().enumerate() {
        let value = if index + 1 < names.len() {
            let (field, after) = take_ifs_field(rest, &ifs);
            rest = after;
            field
        } else {
            last_field(rest, &ifs)
        };
        assign_target(name, &value, state);
    }
    status
}

/// IFS whitespace: space, tab and newline, when IFS contains them.
fn is_ifs_whitespace(c: char, ifs: &str) -> bool {
    matches!(c, ' ' | '\t' | '\n') && ifs.contains(c)
}

/// Characters read by `read`, each marked when a backslash escaped it (an
/// escaped character is never a field separator).
type ReadChars = [(char, bool)];

fn trim_leading_ifs_whitespace<'a>(input: &'a ReadChars, ifs: &str) -> &'a ReadChars {
    let start = input
        .iter()
        .position(|&(c, escaped)| escaped || !is_ifs_whitespace(c, ifs))
        .unwrap_or(input.len());
    &input[start..]
}

/// Bash's `get_word_from_string`: the next field, then the separator after
/// it (surrounding IFS whitespace plus at most one other IFS character).
fn take_ifs_field<'a>(input: &'a ReadChars, ifs: &str) -> (String, &'a ReadChars) {
    let input = trim_leading_ifs_whitespace(input, ifs);
    let end = input
        .iter()
        .position(|&(c, escaped)| !escaped && ifs.contains(c))
        .unwrap_or(input.len());
    let field: String = input[..end].iter().map(|&(c, _)| c).collect();
    let mut rest = trim_leading_ifs_whitespace(&input[end..], ifs);
    if let Some(&(c, false)) = rest.first() {
        if ifs.contains(c) && !is_ifs_whitespace(c, ifs) {
            rest = trim_leading_ifs_whitespace(&rest[1..], ifs);
        }
    }
    (field, rest)
}

/// The value of the last name: everything left, separators included, minus
/// trailing IFS whitespace. When what is left is a single field followed by
/// its separator, that separator goes too (`IFS=, read a <<< 'x,'` gives `x`).
fn last_field(input: &ReadChars, ifs: &str) -> String {
    if input.is_empty() {
        return String::new();
    }
    let (field, rest) = take_ifs_field(input, ifs);
    if rest.is_empty() {
        return field;
    }
    let end = input
        .iter()
        .rposition(|&(c, escaped)| escaped || !is_ifs_whitespace(c, ifs))
        .map_or(0, |last| last + 1);
    input[..end].iter().map(|&(c, _)| c).collect()
}

/// Read one record: up to the delimiter (unless `-N`), `count` characters, end
/// of input, the deadline, or a signal. Without `-r`, a backslash escapes the
/// next character and a backslash-newline pair disappears. Returns the
/// characters read and the status: 0, 1 at end of input, 142 on timeout, or
/// another status for an error or signal.
fn read_input(
    options: &ReadOptions,
    deadline: Option<std::time::Instant>,
) -> (Vec<(char, bool)>, i32) {
    let mut chars: Vec<(char, bool)> = Vec::new();
    let mut bytes_read = 0usize;
    let mut escape_pending = false;
    let status = loop {
        if options.count.is_some_and(|count| chars.len() >= count) {
            break 0;
        }
        let c = match read_char(options.fd, deadline) {
            Ok(Some((c, width))) => {
                bytes_read += width;
                c
            }
            Ok(None) => break 1,
            Err(status) => break status,
        };
        if bytes_read > MAX_READ_BYTES {
            eprintln!("jsh: read: input exceeds {MAX_READ_BYTES} bytes");
            break 1;
        }
        if escape_pending {
            escape_pending = false;
            // A backslash-newline is a line continuation.
            if c != '\n' {
                chars.push((c, true));
            }
            continue;
        }
        if c == '\\' && !options.raw {
            escape_pending = true;
            continue;
        }
        if c == options.delimiter && !options.exact {
            break 0;
        }
        chars.push((c, false));
    };
    (chars, status)
}

/// Read one character from `fd`, a byte at a time so nothing beyond it is
/// consumed (the next command reading the same descriptor must see the rest).
/// A byte that does not start valid UTF-8 counts as one character and reads
/// as U+FFFD. Returns the character and its width in bytes, or `None` at end
/// of input.
fn read_char(
    fd: std::os::fd::RawFd,
    deadline: Option<std::time::Instant>,
) -> Result<Option<(char, usize)>, i32> {
    let mut first = [0u8; 1];
    if read_fd_interruptibly(fd, &mut first, deadline)? == 0 {
        return Ok(None);
    }
    let width = match first[0] {
        0x00..=0x7f => return Ok(Some((first[0] as char, 1))),
        0xc2..=0xdf => 2,
        0xe0..=0xef => 3,
        0xf0..=0xf4 => 4,
        _ => return Ok(Some((char::REPLACEMENT_CHARACTER, 1))),
    };
    let mut buffer = [first[0], 0, 0, 0];
    let mut filled = 1;
    while filled < width {
        let mut next = [0u8; 1];
        if read_fd_interruptibly(fd, &mut next, deadline)? == 0 {
            break;
        }
        buffer[filled] = next[0];
        filled += 1;
        if next[0] & 0xc0 != 0x80 {
            break;
        }
    }
    let c = std::str::from_utf8(&buffer[..filled])
        .ok()
        .and_then(|text| text.chars().next())
        .unwrap_or(char::REPLACEMENT_CHARACTER);
    Ok(Some((c, filled)))
}

/// Restores the exact terminal mode even when a silent read exits early.
struct EchoGuard {
    fd: std::os::fd::RawFd,
    original: nix::sys::termios::Termios,
}

impl EchoGuard {
    fn disable(fd: std::os::fd::RawFd) -> Option<Self> {
        use nix::sys::termios::{tcgetattr, tcsetattr, LocalFlags, SetArg};

        // SAFETY: the descriptor is only borrowed for these two calls.
        let borrowed = unsafe { std::os::fd::BorrowedFd::borrow_raw(fd) };
        let original = tcgetattr(borrowed).ok()?;
        let mut hidden = original.clone();
        hidden.local_flags.remove(LocalFlags::ECHO);
        tcsetattr(borrowed, SetArg::TCSANOW, &hidden).ok()?;
        Some(Self { fd, original })
    }
}

impl Drop for EchoGuard {
    fn drop(&mut self) {
        // SAFETY: the descriptor outlives the read that created the guard.
        let borrowed = unsafe { std::os::fd::BorrowedFd::borrow_raw(self.fd) };
        let _ = nix::sys::termios::tcsetattr(
            borrowed,
            nix::sys::termios::SetArg::TCSANOW,
            &self.original,
        );
    }
}

const READ_TIMEOUT_STATUS: i32 = 128 + nix::libc::SIGALRM;

fn parse_read_timeout(value: &str) -> Option<std::time::Duration> {
    let number = value.strip_prefix(['+', '-']).unwrap_or(value);
    if number.is_empty()
        || !number.bytes().any(|byte| byte.is_ascii_digit())
        || number.bytes().filter(|&byte| byte == b'.').count() > 1
        || !number
            .bytes()
            .all(|byte| byte.is_ascii_digit() || byte == b'.')
    {
        return None;
    }
    let nonzero = number
        .bytes()
        .any(|byte| byte.is_ascii_digit() && byte != b'0');
    if value.starts_with('-') && nonzero {
        return None;
    }
    let seconds = value.parse::<f64>().ok()?;
    if !seconds.is_finite() || seconds < 0.0 {
        return None;
    }
    let duration = std::time::Duration::try_from_secs_f64(seconds).ok()?;
    Some(if nonzero && duration.is_zero() {
        std::time::Duration::from_nanos(1)
    } else {
        duration
    })
}

fn poll_read_fd(fd: std::os::fd::RawFd, timeout_ms: i32) -> Result<bool, i32> {
    if let Some(status) = crate::signal::pending_status() {
        return Err(status);
    }
    let mut descriptor = nix::libc::pollfd {
        fd,
        events: nix::libc::POLLIN,
        revents: 0,
    };
    let result = unsafe { nix::libc::poll(&mut descriptor, 1, timeout_ms) };
    if let Some(status) = crate::signal::pending_status() {
        return Err(status);
    }
    if result < 0 {
        if std::io::Error::last_os_error().kind() == std::io::ErrorKind::Interrupted {
            // The caller recomputes the remaining absolute deadline.
            return Ok(false);
        }
        return Err(1);
    }
    if descriptor.revents & (nix::libc::POLLERR | nix::libc::POLLNVAL) != 0 {
        return Err(1);
    }
    Ok(descriptor.revents & (nix::libc::POLLIN | nix::libc::POLLHUP) != 0)
}

/// Read only the requested bytes from `fd`. A buffered reader may prefetch
/// bytes past a delimiter/count, hiding input from the next external command
/// that inherits the same descriptor.
fn read_fd_interruptibly(
    fd: std::os::fd::RawFd,
    buffer: &mut [u8],
    deadline: Option<std::time::Instant>,
) -> Result<usize, i32> {
    loop {
        if let Some(status) = crate::signal::pending_status() {
            return Err(status);
        }
        if let Some(deadline) = deadline {
            let now = std::time::Instant::now();
            if now >= deadline {
                return Err(READ_TIMEOUT_STATUS);
            }
            let remaining = deadline.duration_since(now);
            let millis = remaining
                .as_millis()
                .saturating_add(u128::from(remaining.subsec_nanos() % 1_000_000 != 0));
            if !poll_read_fd(fd, millis.min(100) as i32)? {
                continue;
            }
            if std::time::Instant::now() >= deadline {
                return Err(READ_TIMEOUT_STATUS);
            }
        }
        let borrowed = unsafe { std::os::fd::BorrowedFd::borrow_raw(fd) };
        match nix::unistd::read(borrowed, buffer) {
            Ok(read) => return Ok(read),
            Err(nix::errno::Errno::EINTR) => {
                if let Some(status) = crate::signal::pending_status() {
                    return Err(status);
                }
            }
            Err(_) => return Err(1),
        }
    }
}

fn builtin_test(name: &str, args: &[String], state: &mut ShellState) -> i32 {
    // Only `[` has a closing delimiter. A literal `]` remains ordinary data
    // for `test`, and for `[` everywhere except its final argument.
    let args = if name == "[" {
        match args.split_last() {
            Some((last, rest)) if last == "]" => rest,
            _ => {
                eprintln!("jsh: [: missing ']'");
                return 2;
            }
        }
    } else {
        args
    };
    let args: Vec<&str> = args.iter().map(String::as_str).collect();
    match evaluate_test(&args, state) {
        TestResult::True => 0,
        TestResult::False => 1,
        TestResult::Error(message) => {
            eprintln!(
                "jsh: {name}: {}",
                message.as_deref().unwrap_or("invalid expression")
            );
            2
        }
    }
}

#[derive(Debug, Clone)]
enum TestResult {
    True,
    False,
    /// A malformed expression. The message, when present, names the
    /// offending operand the way bash does (`x: integer expression expected`).
    Error(Option<String>),
}

impl TestResult {
    fn from_bool(value: bool) -> Self {
        if value {
            Self::True
        } else {
            Self::False
        }
    }

    fn negate(self) -> Self {
        match self {
            Self::True => Self::False,
            Self::False => Self::True,
            error @ Self::Error(_) => error,
        }
    }
}

fn evaluate_test(args: &[&str], state: &mut ShellState) -> TestResult {
    // Small-argument rules disambiguate strings that happen to be operators:
    // `test !` tests a nonempty string, and `test ! = !` compares strings.
    match args {
        [] => return TestResult::False,
        [value] => return TestResult::from_bool(!value.is_empty()),
        ["!", rest] => return evaluate_test(&[rest], state).negate(),
        [_, op, _] if is_test_binary_op(op) => return parse_test_binary(args, 0).0,
        [left, "-a", right] => return TestResult::from_bool(!left.is_empty() && !right.is_empty()),
        [left, "-o", right] => return TestResult::from_bool(!left.is_empty() || !right.is_empty()),
        ["(", value, ")"] => return evaluate_test(&[value], state),
        ["!", "!", _] => return TestResult::Error(None),
        ["!", rest @ ..] if args.len() <= 4 => return evaluate_test(rest, state).negate(),
        ["(", inner @ .., ")"] if args.len() == 4 => return evaluate_test(inner, state),
        _ => {}
    }
    let (result, consumed) = parse_test_expr(args, 0, state);
    if consumed == args.len() || matches!(result, TestResult::Error(_)) {
        result
    } else {
        TestResult::Error(None)
    }
}

fn is_test_binary_op(op: &str) -> bool {
    matches!(
        op,
        "=" | "=="
            | "!="
            | "<"
            | ">"
            | "-eq"
            | "-ne"
            | "-lt"
            | "-le"
            | "-gt"
            | "-ge"
            | "-nt"
            | "-ot"
            | "-ef"
    )
}

fn parse_test_expr(args: &[&str], idx: usize, state: &mut ShellState) -> (TestResult, usize) {
    parse_or_expr(args, idx, state)
}

fn parse_or_expr(args: &[&str], idx: usize, state: &mut ShellState) -> (TestResult, usize) {
    let (mut left, mut new_idx) = parse_and_expr(args, idx, state);

    while new_idx < args.len() && args[new_idx] == "-o" {
        new_idx += 1;
        let (right, next_idx) = parse_and_expr(args, new_idx, state);

        left = match (left, right) {
            (error @ TestResult::Error(_), _) | (_, error @ TestResult::Error(_)) => error,
            (TestResult::True, _) | (_, TestResult::True) => TestResult::True,
            (TestResult::False, TestResult::False) => TestResult::False,
        };
        new_idx = next_idx;
    }

    (left, new_idx)
}

fn parse_and_expr(args: &[&str], idx: usize, state: &mut ShellState) -> (TestResult, usize) {
    let (mut left, mut new_idx) = parse_primary(args, idx, state);

    while new_idx < args.len() && args[new_idx] == "-a" {
        new_idx += 1;
        let (right, next_idx) = parse_primary(args, new_idx, state);

        left = match (left, right) {
            (error @ TestResult::Error(_), _) | (_, error @ TestResult::Error(_)) => error,
            (TestResult::False, _) | (_, TestResult::False) => TestResult::False,
            (TestResult::True, TestResult::True) => TestResult::True,
        };
        new_idx = next_idx;
    }

    (left, new_idx)
}

fn parse_primary(args: &[&str], idx: usize, state: &mut ShellState) -> (TestResult, usize) {
    if idx >= args.len() {
        return (TestResult::Error(None), idx);
    }

    // Binary comparisons take precedence over interpreting the left operand
    // as a unary operator or a negation token.
    if idx + 2 < args.len() && is_test_binary_op(args[idx + 1]) {
        return parse_test_binary(args, idx);
    }

    // Handle negation
    if args[idx] == "!" {
        let (result, new_idx) = parse_primary(args, idx + 1, state);
        return (result.negate(), new_idx);
    }

    // Handle parentheses
    if args[idx] == "(" {
        let (result, new_idx) = parse_or_expr(args, idx + 1, state);
        if new_idx < args.len() && args[new_idx] == ")" {
            return (result, new_idx + 1);
        }
        return (TestResult::Error(None), new_idx);
    }

    if idx + 1 < args.len() {
        if let Some(result) = unary_test(args[idx], args[idx + 1], state) {
            return (TestResult::from_bool(result), idx + 2);
        }
    }

    // A bare string may end at a logical operator or closing parenthesis.
    if idx + 1 == args.len() || matches!(args.get(idx + 1), Some(&"-a" | &"-o" | &")")) {
        return (TestResult::from_bool(!args[idx].is_empty()), idx + 1);
    }

    (TestResult::Error(None), idx)
}

fn parse_test_binary(args: &[&str], idx: usize) -> (TestResult, usize) {
    let (left, op, right) = (args[idx], args[idx + 1], args[idx + 2]);
    let result = match op {
        "=" | "==" => TestResult::from_bool(left == right),
        "!=" => TestResult::from_bool(left != right),
        "<" => TestResult::from_bool(left < right),
        ">" => TestResult::from_bool(left > right),
        "-eq" | "-ne" | "-lt" | "-le" | "-gt" | "-ge" => {
            match (test_integer(left), test_integer(right)) {
                (Some(a), Some(b)) => TestResult::from_bool(compare_integers(op, a, b)),
                (None, _) => {
                    TestResult::Error(Some(format!("{left}: integer expression expected")))
                }
                (_, None) => {
                    TestResult::Error(Some(format!("{right}: integer expression expected")))
                }
            }
        }
        _ => match binary_file_test(op, left, right) {
            Some(result) => TestResult::from_bool(result),
            None => TestResult::Error(None),
        },
    };
    (result, idx + 3)
}

/// `test`'s integer operand, as bash's `legal_number` reads it: optional
/// blanks around an optionally signed decimal number that fits in 64 bits.
/// `010` is ten here; only `[[ ]]` evaluates operands arithmetically.
fn test_integer(text: &str) -> Option<i64> {
    text.trim_matches([' ', '\t']).parse::<i64>().ok()
}

fn compare_integers(op: &str, a: i64, b: i64) -> bool {
    match op {
        "-eq" => a == b,
        "-ne" => a != b,
        "-lt" => a < b,
        "-le" => a <= b,
        "-gt" => a > b,
        _ => a >= b,
    }
}

/// The unary primaries `test`, `[` and `[[ ]]` share. `None` means `op` is
/// not a unary operator, so the caller can treat it as an ordinary string.
fn unary_test(op: &str, operand: &str, state: &mut ShellState) -> Option<bool> {
    use std::os::unix::fs::{FileTypeExt, MetadataExt, PermissionsExt};
    // Everything except -h/-L follows symbolic links, like stat(2).
    let metadata = || std::fs::metadata(operand).ok();
    let mode_bit = |bit: u32| metadata().is_some_and(|m| m.permissions().mode() & bit != 0);
    Some(match op {
        "-n" => !operand.is_empty(),
        "-z" => operand.is_empty(),
        "-a" | "-e" => metadata().is_some(),
        "-f" => metadata().is_some_and(|m| m.is_file()),
        "-d" => metadata().is_some_and(|m| m.is_dir()),
        "-b" => metadata().is_some_and(|m| m.file_type().is_block_device()),
        "-c" => metadata().is_some_and(|m| m.file_type().is_char_device()),
        "-p" => metadata().is_some_and(|m| m.file_type().is_fifo()),
        "-S" => metadata().is_some_and(|m| m.file_type().is_socket()),
        "-h" | "-L" => is_symlink(operand),
        "-s" => metadata().is_some_and(|m| m.len() > 0),
        "-g" => mode_bit(0o2000),
        "-u" => mode_bit(0o4000),
        "-k" => mode_bit(0o1000),
        "-r" => is_readable(operand),
        "-w" => is_writable(operand),
        "-x" => is_executable(operand),
        "-O" => metadata().is_some_and(|m| m.uid() == nix::unistd::geteuid().as_raw()),
        "-G" => metadata().is_some_and(|m| m.gid() == nix::unistd::getegid().as_raw()),
        // Modified since it was last read.
        "-N" => {
            metadata().is_some_and(|m| (m.mtime(), m.mtime_nsec()) > (m.atime(), m.atime_nsec()))
        }
        "-t" => test_integer(operand)
            .and_then(|fd| i32::try_from(fd).ok())
            .is_some_and(|fd| {
                nix::unistd::isatty(unsafe {
                    // SAFETY: isatty only inspects the descriptor; an invalid one
                    // reports EBADF, which reads as false.
                    std::os::fd::BorrowedFd::borrow_raw(fd)
                })
                .unwrap_or(false)
            }),
        "-o" => set_option_is_known(operand) && shell_option_enabled(state, operand),
        "-v" => variable_is_set(operand, state),
        "-R" => is_nameref(operand, state),
        _ => return None,
    })
}

/// `-nt`, `-ot` and `-ef`. A missing file is older than any existing one.
fn binary_file_test(op: &str, left: &str, right: &str) -> Option<bool> {
    use std::os::unix::fs::MetadataExt;
    let left_meta = std::fs::metadata(left).ok();
    let right_meta = std::fs::metadata(right).ok();
    let mtime = |m: &std::fs::Metadata| (m.mtime(), m.mtime_nsec());
    Some(match op {
        "-nt" => match (&left_meta, &right_meta) {
            (Some(l), Some(r)) => mtime(l) > mtime(r),
            (Some(_), None) => true,
            _ => false,
        },
        "-ot" => match (&left_meta, &right_meta) {
            (Some(l), Some(r)) => mtime(l) < mtime(r),
            (None, Some(_)) => true,
            _ => false,
        },
        "-ef" => match (&left_meta, &right_meta) {
            (Some(l), Some(r)) => l.dev() == r.dev() && l.ino() == r.ino(),
            _ => false,
        },
        _ => return None,
    })
}

/// `-v NAME`, `-v NAME[SUBSCRIPT]`: is the variable (or element) set, even to
/// the empty string? A bare array name asks about element 0, as in bash, and
/// `NAME[@]` about any element. An indexed subscript is an arithmetic
/// expression (`arr[i+1]`, `arr[-1]`); an associative one is a key.
fn variable_is_set(name: &str, state: &mut ShellState) -> bool {
    let (base, subscript) = match name.find('[') {
        Some(bracket) if name.ends_with(']') && bracket + 1 < name.len() - 1 => {
            (&name[..bracket], Some(&name[bracket + 1..name.len() - 1]))
        }
        Some(_) => return false,
        None => (name, None),
    };
    if subscript.is_none() && !base.is_empty() && base.bytes().all(|b| b.is_ascii_digit()) {
        return match base.parse::<usize>() {
            Ok(0) => true,
            Ok(n) => n <= state.positional_params.len(),
            Err(_) => false,
        };
    }
    if !crate::environment::is_valid_identifier(base) {
        return false;
    }
    let is_array = state.is_array(base);
    let Some(subscript) = subscript else {
        return if is_array {
            state.array_keys(base).iter().any(|key| key == "0")
        } else {
            state.get_var(base).is_some()
        };
    };
    if subscript == "@" || subscript == "*" {
        return if is_array {
            state.array_length(base) > 0
        } else {
            state.get_var(base).is_some()
        };
    }
    let keys = if is_array {
        state.array_keys(base)
    } else if state.get_var(base).is_some() {
        // A scalar is a one-element array.
        vec!["0".to_string()]
    } else {
        Vec::new()
    };
    if keys.iter().any(|key| key == subscript) {
        return true;
    }
    // Only indexed arrays have purely numeric keys; anything else is
    // associative, and its subscript is a key that was not found.
    let mut max_index: Option<i64> = None;
    for key in &keys {
        match key.parse::<i64>() {
            Ok(index) => max_index = Some(max_index.map_or(index, |max| max.max(index))),
            Err(_) => return false,
        }
    }
    let saved_error = state.expansion_error.take();
    let value = crate::expand::expand_arithmetic(subscript, state);
    // A bad subscript makes the answer "not set", not a failed command.
    let failed = std::mem::replace(&mut state.expansion_error, saved_error).is_some();
    let Ok(mut index) = value.parse::<i64>() else {
        return false;
    };
    if failed {
        return false;
    }
    if index < 0 {
        // Negative subscripts count back from one past the highest index.
        index = max_index.map_or(index, |max| max.saturating_add(1).saturating_add(index));
    }
    keys.iter().any(|key| *key == index.to_string())
}

/// `-R NAME`: is NAME a nameref? jsh has no namerefs (`declare -n`), so no
/// variable ever is one.
fn is_nameref(name: &str, state: &ShellState) -> bool {
    state
        .find_variable(name)
        .is_some_and(|var| var.attrs.contains(crate::variables::VarAttrs::NAMEREF))
}

fn is_symlink(path: &str) -> bool {
    std::fs::symlink_metadata(path)
        .map(|m| m.file_type().is_symlink())
        .unwrap_or(false)
}

/// Permission probe shared by `test`/`[` and `[[ ]]`, so the two can never
/// disagree again (`[[ -x /etc/passwd ]]` used to be a plain existence check).
///
/// `-r`/`-w`/`-x` must answer for the EFFECTIVE user, like bash's `sh_eaccess`:
/// plain `access(2)` asks about the REAL uid, which is the wrong answer in a
/// setuid or `su`-style context. `faccessat(AT_EACCESS)` is the portable
/// spelling of `eaccess`, which nix only exposes on Linux/FreeBSD.
///
/// Root is the case worth naming: the kernel grants root R_OK/W_OK on every
/// file and X_OK whenever *any* execute bit is set, so `[[ -w /etc/passwd ]]`
/// is legitimately true for root and false for everyone else. Differential
/// tests against bash therefore have to run as the same user.
fn access_ok(path: &str, mode: nix::unistd::AccessFlags) -> bool {
    use nix::fcntl::AtFlags;
    use std::os::fd::BorrowedFd;
    // SAFETY: AT_FDCWD is a valid special descriptor for the *at syscalls and is
    // only borrowed for the duration of the call.
    let cwd = unsafe { BorrowedFd::borrow_raw(nix::libc::AT_FDCWD) };
    nix::unistd::faccessat(cwd, std::path::Path::new(path), mode, AtFlags::AT_EACCESS).is_ok()
}

fn is_readable(path: &str) -> bool {
    access_ok(path, nix::unistd::AccessFlags::R_OK)
}

fn is_writable(path: &str) -> bool {
    access_ok(path, nix::unistd::AccessFlags::W_OK)
}

fn is_executable(path: &str) -> bool {
    access_ok(path, nix::unistd::AccessFlags::X_OK)
}

/// Every `set -o` name bash knows, in the order bash lists them, paired with
/// its short flag letter. jsh enforces a subset (errexit, nounset, xtrace,
/// noglob, pipefail, globstar, vi/emacs); the rest are remembered in
/// `shell_opts.tracked_opts` so they are neither silently mistaken for
/// positional parameters nor lost across `eval "$(set +o)"`.
///
/// `errtrace` remains stored in the generic tracked map, but command
/// substitution enforces its ERR-trap inheritance effect. Remembered but not
/// otherwise enforced today: allexport, braceexpand, functrace, hashall,
/// histexpand, history, ignoreeof, interactive-comments, keyword, monitor,
/// noclobber, noexec, nolog, notify, onecmd, physical, posix, privileged,
/// verbose. Rejecting them instead would be worse: bash accepts them, so
/// `set -a` in a real script must not become a hard error.
pub(crate) const SET_OPTIONS: &[(&str, Option<char>)] = &[
    ("allexport", Some('a')),
    ("braceexpand", Some('B')),
    ("emacs", None),
    ("errexit", Some('e')),
    ("errtrace", Some('E')),
    ("functrace", Some('T')),
    ("globstar", None), // jsh extension: bash exposes globstar via shopt only
    ("hashall", Some('h')),
    ("histexpand", Some('H')),
    ("history", None),
    ("ignoreeof", None),
    ("interactive-comments", None),
    ("keyword", Some('k')),
    ("monitor", Some('m')),
    ("noclobber", Some('C')),
    ("noexec", Some('n')),
    ("noglob", Some('f')),
    ("nolog", None),
    ("notify", Some('b')),
    ("nounset", Some('u')),
    ("onecmd", Some('t')),
    ("physical", Some('P')),
    ("pipefail", None),
    ("posix", None),
    ("privileged", Some('p')),
    ("verbose", Some('v')),
    ("vi", None),
    ("xtrace", Some('x')),
];

const SET_USAGE: &str = "set: usage: set [-abefhkmnptuvxBCEHPT] [-o option-name] [--] [arg ...]";

fn set_option_name_for_flag(c: char) -> Option<&'static str> {
    SET_OPTIONS
        .iter()
        .find(|(_, flag)| *flag == Some(c))
        .map(|(name, _)| *name)
}

fn set_option_is_known(name: &str) -> bool {
    SET_OPTIONS.iter().any(|(n, _)| *n == name)
}

/// Apply one `set -o NAME` / `set +o NAME`.
fn apply_shell_option(state: &mut ShellState, name: &str, enable: bool) {
    match name {
        "errexit" => state.shell_opts.errexit = enable,
        "nounset" => state.shell_opts.nounset = enable,
        "xtrace" => state.shell_opts.xtrace = enable,
        "noclobber" => state.shell_opts.noclobber = enable,
        "pipefail" => state.shell_opts.pipefail = enable,
        "noglob" => state.shell_opts.noglob = enable,
        "globstar" => state.shell_opts.globstar = enable,
        "vi" => {
            state.editing_mode = if enable {
                crate::environment::EditingMode::Vi
            } else {
                crate::environment::EditingMode::Emacs
            }
        }
        "emacs" => {
            state.editing_mode = if enable {
                crate::environment::EditingMode::Emacs
            } else {
                crate::environment::EditingMode::Vi
            }
        }
        other => {
            state
                .shell_opts
                .tracked_opts
                .insert(other.to_string(), enable);
        }
    }
}

fn shell_option_enabled(state: &ShellState, name: &str) -> bool {
    match name {
        "errexit" => state.shell_opts.errexit,
        "nounset" => state.shell_opts.nounset,
        "xtrace" => state.shell_opts.xtrace,
        "noclobber" => state.shell_opts.noclobber,
        "pipefail" => state.shell_opts.pipefail,
        "noglob" => state.shell_opts.noglob,
        "globstar" => state.shell_opts.globstar,
        "vi" => state.editing_mode == crate::environment::EditingMode::Vi,
        "emacs" => state.editing_mode == crate::environment::EditingMode::Emacs,
        other => match state.shell_opts.tracked_opts.get(other) {
            Some(&enabled) => enabled,
            // Defaults for the options jsh only remembers, as bash reports them.
            None => match other {
                "braceexpand" | "hashall" | "interactive-comments" => true,
                "history" | "monitor" => state.interactive,
                _ => false,
            },
        },
    }
}

fn builtin_set(args: &[String], state: &mut ShellState) -> i32 {
    if args.is_empty() {
        for (name, _, var) in state.visible_variables() {
            if !crate::environment::is_valid_identifier(name) {
                continue;
            }
            if let Some(line) = crate::var_builtins::set_style_line(name, var) {
                println!("{}", line);
            }
        }
        return 0;
    }

    // Bash parses the whole option list before it changes anything: `set -e -q`
    // reports the bad option and leaves errexit OFF. Collect the requested
    // changes first, then apply them, so a typo cannot half-apply.
    let mut pending: Vec<(&'static str, bool)> = Vec::new();
    let mut list_long = false; // `set -o` with no name
    let mut list_short = false; // `set +o` with no name
    let mut operand_start: Option<usize> = None;
    let mut i = 0;
    while i < args.len() {
        let arg = args[i].as_str();
        // `--` ends option parsing; the rest become the positional parameters
        // even when there is nothing left (`set --` clears them).
        if arg == "--" {
            operand_start = Some(i + 1);
            break;
        }
        // A lone `-` also ends option parsing and, as a bash special case,
        // turns xtrace and verbose off. A lone `+` just ends option parsing.
        if arg == "-" || arg == "+" {
            if arg == "-" {
                pending.push(("xtrace", false));
                pending.push(("verbose", false));
            }
            operand_start = Some(i + 1);
            break;
        }
        let mut chars = arg.chars();
        let sign = chars.next().unwrap_or(' ');
        if sign != '-' && sign != '+' {
            // First operand: everything from here on is a positional parameter.
            operand_start = Some(i);
            break;
        }
        let enable = sign == '-';
        // Short options cluster: `-euo` is `-e -u -o`.
        for c in chars {
            if c == 'o' {
                // `-o` takes the next word as the option name, but only when
                // that word is not itself an option; `set -o` alone lists.
                let next_is_name = args
                    .get(i + 1)
                    .map(|n| !n.starts_with('-') && !n.starts_with('+'))
                    .unwrap_or(false);
                if next_is_name {
                    i += 1;
                    let name = args[i].as_str();
                    if !set_option_is_known(name) {
                        eprintln!("jsh: set: {}: invalid option name", name);
                        return 2;
                    }
                    let canonical = SET_OPTIONS
                        .iter()
                        .find(|(n, _)| *n == name)
                        .map(|(n, _)| *n)
                        .unwrap();
                    pending.push((canonical, enable));
                } else if enable {
                    list_long = true;
                } else {
                    list_short = true;
                }
                continue;
            }
            match set_option_name_for_flag(c) {
                Some(name) => pending.push((name, enable)),
                None => {
                    // Loud failure, like bash. Never reinterpret an unknown
                    // flag as a positional parameter: that turned every
                    // `set -euo pipefail` into a silent no-op.
                    eprintln!("jsh: set: -{}: invalid option", c);
                    eprintln!("{}", SET_USAGE);
                    return 2;
                }
            }
        }
        i += 1;
    }

    for (name, enable) in pending {
        apply_shell_option(state, name, enable);
    }
    if list_long {
        for (name, _) in SET_OPTIONS {
            println!(
                "{:<15}\t{}",
                name,
                if shell_option_enabled(state, name) {
                    "on"
                } else {
                    "off"
                }
            );
        }
    }
    if list_short {
        for (name, _) in SET_OPTIONS {
            println!(
                "set {}o {}",
                if shell_option_enabled(state, name) {
                    "-"
                } else {
                    "+"
                },
                name
            );
        }
    }
    if let Some(start) = operand_start {
        state.positional_params = args[start..].to_vec();
    }
    0
}

fn builtin_local(args: &[String], state: &mut ShellState) -> i32 {
    // `local -`: the function's `set` options are restored on return. The
    // variable side of `local` treats `-` as a no-op operand.
    if args.iter().any(|arg| arg == "-") && !state.save_options_for_function() {
        eprintln!("jsh: local: can only be used in a function");
        return 1;
    }
    crate::var_builtins::local(args, state)
}

fn builtin_history(_state: &ShellState) -> i32 {
    for (i, entry) in crate::history::History::load_default_entries(usize::MAX)
        .iter()
        .enumerate()
    {
        println!("{:5}  {}", i + 1, entry.command);
    }
    0
}

/// Upper bound on what `printf -v` collects in memory. Streaming output has
/// no such limit, but a variable has to hold the whole result at once, and a
/// format repeated over many arguments with wide fields could otherwise grow
/// without bound.
const MAX_PRINTF_VAR_BYTES: usize = 64 * 1024 * 1024;

/// A `Write` sink for `printf -v` that refuses to grow past its limit.
struct BoundedBuffer {
    bytes: Vec<u8>,
    limit: usize,
}

impl std::io::Write for BoundedBuffer {
    fn write(&mut self, data: &[u8]) -> std::io::Result<usize> {
        if self.bytes.len().saturating_add(data.len()) > self.limit {
            return Err(std::io::Error::other(format!(
                "-v: output exceeds {} bytes",
                self.limit
            )));
        }
        self.bytes.extend_from_slice(data);
        Ok(data.len())
    }

    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}

/// Split `name[subscript]` into its parts; a plain name has no subscript.
/// `None` when the text is neither a valid name nor a valid element reference.
fn variable_target(target: &str) -> Option<(&str, Option<&str>)> {
    let (name, subscript) = match target.find('[') {
        Some(open) if target.ends_with(']') && open + 1 < target.len() - 1 => {
            (&target[..open], Some(&target[open + 1..target.len() - 1]))
        }
        Some(_) => return None,
        None => (target, None),
    };
    crate::environment::is_valid_identifier(name).then_some((name, subscript))
}

/// Assign to a variable or array element named by a builtin's operand (`read
/// 'a[1]'`, `printf -v 'a[1]'`), exactly as the assignment `a[1]=value` would.
fn assign_target(target: &str, value: &str, state: &mut ShellState) -> bool {
    match variable_target(target) {
        Some((name, Some(subscript))) => {
            state.set_array_element(name, subscript, value);
            true
        }
        Some((name, None)) => {
            state.set_var(name, value);
            true
        }
        None => false,
    }
}

fn builtin_printf(args: &[String], state: &mut ShellState) -> i32 {
    use std::io::Write;
    let mut target: Option<String> = None;
    let mut args = args;
    while let Some(option) = args.first() {
        if option == "--" {
            args = &args[1..];
            break;
        }
        if option == "-v" {
            let Some(name) = args.get(1) else {
                eprintln!("jsh: printf: -v: option requires an argument");
                eprintln!("printf: usage: printf [-v var] format [arguments]");
                return 2;
            };
            target = Some(name.clone());
            args = &args[2..];
        } else if let Some(name) = option.strip_prefix("-v") {
            target = Some(name.to_string());
            args = &args[1..];
        } else if option.starts_with('-') && option.len() > 1 {
            eprintln!(
                "jsh: printf: {option}: invalid option; use -- before a format beginning with '-'"
            );
            eprintln!("printf: usage: printf [-v var] format [arguments]");
            return 2;
        } else {
            break;
        }
    }
    if let Some(name) = &target {
        if variable_target(name).is_none() {
            eprintln!("jsh: printf: `{name}': not a valid identifier");
            return 2;
        }
    }
    let Some(format) = args.first() else {
        eprintln!("jsh: printf: usage: printf [-v var] format [arguments]");
        return 2;
    };
    let context = crate::printf::Context {
        shell_start: state
            .builtin_state
            .start_time
            .duration_since(std::time::UNIX_EPOCH)
            .map_or(0, |elapsed| elapsed.as_secs() as i64),
    };
    let report = |message| eprintln!("jsh: printf: {message}");

    if let Some(name) = target {
        let mut buffer = BoundedBuffer {
            bytes: Vec::new(),
            limit: MAX_PRINTF_VAR_BYTES,
        };
        return match crate::printf::write_printf(&mut buffer, format, &args[1..], context, report) {
            Ok(status) => {
                let value = String::from_utf8_lossy(&buffer.bytes).into_owned();
                assign_target(&name, &value, state);
                status
            }
            Err(error) => {
                eprintln!("jsh: printf: {error}");
                1
            }
        };
    }

    let stdout = std::io::stdout();
    let mut output = std::io::BufWriter::new(stdout.lock());
    let result = crate::printf::write_printf(&mut output, format, &args[1..], context, report);
    // Preserve output before a bad conversion and report write failures without
    // a print!/stdout panic (including a closed downstream pipe).
    let flushed = output.flush();
    match (result, flushed) {
        (Ok(status), Ok(())) => status,
        (Err(error), _) => {
            eprintln!("jsh: printf: {error}");
            1
        }
        (_, Err(error)) => {
            eprintln!("jsh: printf: {error}");
            1
        }
    }
}

/// Bash `let`: evaluate each argument as an arithmetic expression. The status
/// is 0 when the last value is non-zero and 1 otherwise; an expression that
/// fails reports an error and returns 1 without evaluating the rest.
///
/// The arguments were already expanded as ordinary words, so they are not
/// expanded again. jsh's typed `let NAME = EXPR` binding never reaches here:
/// the executor intercepts it before word expansion.
fn builtin_let(args: &[String], state: &mut ShellState) -> i32 {
    let args = match args.first() {
        Some(first) if first == "--" => &args[1..],
        _ => args,
    };
    if args.is_empty() {
        eprintln!("jsh: let: expression expected");
        return 1;
    }
    let mut last = 0;
    for expression in args {
        match crate::expand::evaluate_arithmetic_text(expression, state) {
            Ok(value) => last = value,
            Err(message) => {
                eprintln!("jsh: let: {message}");
                // `set -u` rejecting an unset variable is fatal, as it is in
                // any expansion; other failures just make `let` return 1.
                if state.expansion_error.take().is_some() && !state.interactive {
                    state.abort_current_program = true;
                }
                return 1;
            }
        }
    }
    i32::from(last == 0)
}

fn builtin_shift(args: &[String], state: &mut ShellState) -> i32 {
    if args.len() > 1 {
        eprintln!("jsh: shift: too many arguments");
        return 1;
    }
    let count = match args.first() {
        Some(value) => match value.parse::<usize>() {
            Ok(count) => count,
            Err(_) => {
                eprintln!("jsh: shift: {}: numeric argument required", value);
                return 1;
            }
        },
        None => 1,
    };
    if count > state.positional_params.len() {
        eprintln!("jsh: shift: shift count out of range");
        return 1;
    }
    state.positional_params.drain(..count);
    0
}

fn builtin_exec(args: &[String], state: &mut ShellState) -> i32 {
    use std::io::Write;
    use std::os::unix::process::CommandExt;
    let mut index = 0;
    let mut clear_environment = false;
    let mut login = false;
    let mut arg0 = None;
    while let Some(option) = args.get(index) {
        if option == "--" {
            index += 1;
            break;
        }
        if !option.starts_with('-') || option == "-" {
            break;
        }
        let mut flags = option[1..].chars();
        while let Some(flag) = flags.next() {
            match flag {
                'c' => clear_environment = true,
                'l' => login = true,
                'a' => {
                    let attached = flags.as_str();
                    if attached.is_empty() {
                        index += 1;
                        let Some(name) = args.get(index) else {
                            eprintln!("jsh: exec: -a requires an argument");
                            return 2;
                        };
                        arg0 = Some(name.clone());
                    } else {
                        arg0 = Some(attached.to_string());
                    }
                    break;
                }
                _ => {
                    eprintln!("jsh: exec: invalid option: {option}");
                    return 2;
                }
            }
        }
        index += 1;
    }
    let Some(program) = args.get(index) else {
        // The executor commits this command's redirections on success.
        return 0;
    };
    let mut command = std::process::Command::new(program);
    command.args(&args[index + 1..]);
    let name = arg0.as_deref().unwrap_or(program);
    if login {
        command.arg0(format!("-{name}"));
    } else if arg0.is_some() {
        command.arg0(name);
    }
    if clear_environment {
        command.env_clear();
    }
    let _ = std::io::stdout().flush();
    let _ = std::io::stderr().flush();
    // Caught handlers reset at exec, but ignored job-control/SIGPIPE handlers
    // do not. Give the replacement the same defaults as an ordinary child,
    // and restore the exact prior dispositions if exec fails and we continue.
    use nix::sys::signal::{self, SaFlags, SigAction, SigHandler, SigSet, Signal};
    let default = SigAction::new(SigHandler::SigDfl, SaFlags::empty(), SigSet::empty());
    let saved: Vec<_> = [
        Signal::SIGINT,
        Signal::SIGHUP,
        Signal::SIGTERM,
        Signal::SIGTSTP,
        Signal::SIGTTIN,
        Signal::SIGTTOU,
        Signal::SIGPIPE,
        Signal::SIGCHLD,
    ]
    .into_iter()
    // `trap '' SIG` carries over to the new program, as in bash.
    .filter(|sig| !crate::signal::is_user_ignored(*sig as i32))
    .filter_map(|sig| {
        unsafe { signal::sigaction(sig, &default) }
            .ok()
            .map(|old| (sig, old))
    })
    .collect();
    let error = command.exec();
    for (sig, old) in saved {
        let _ = unsafe { signal::sigaction(sig, &old) };
    }
    let status = if error.kind() == std::io::ErrorKind::NotFound {
        127
    } else {
        126
    };
    eprintln!("jsh: exec: {program}: {error}");
    if !state.interactive
        && !state
            .shell_opts
            .shopt_opts
            .get("execfail")
            .copied()
            .unwrap_or(false)
    {
        EXIT_CODE.store(status, std::sync::atomic::Ordering::SeqCst);
        EXIT_REQUESTED.store(true, std::sync::atomic::Ordering::SeqCst);
    }
    status
}

// ============================================================
// Directory stack: pushd, popd, dirs
// ============================================================
//
// Bash's stack is the current directory followed by the saved ones:
// `dirs` prints entry 0 (the logical cwd) first. `state.dir_stack` keeps the
// saved entries with the most recent LAST, so stack entry i >= 1 lives at
// `dir_stack[len - i]`.

/// The whole stack, entry 0 first.
fn stack_entries(state: &ShellState) -> Vec<String> {
    let mut entries = vec![logical_cwd(state)];
    entries.extend(
        state
            .dir_stack
            .iter()
            .rev()
            .map(|dir| dir.to_string_lossy().into_owned()),
    );
    entries
}

/// Store entries 1.. back into `dir_stack` (entry 0 is the cwd itself).
fn store_stack(state: &mut ShellState, entries: &[String]) {
    state.dir_stack = entries
        .iter()
        .skip(1)
        .rev()
        .map(std::path::PathBuf::from)
        .collect();
}

/// `+N` (from the left, 0 = current directory) or `-N` (from the right).
#[derive(Clone, Copy)]
enum StackIndex {
    FromTop(usize),
    FromBottom(usize),
}

fn stack_index(spec: &str) -> Option<StackIndex> {
    let digits = &spec.get(1..)?;
    if digits.is_empty() || !digits.bytes().all(|b| b.is_ascii_digit()) {
        return None;
    }
    let n = digits.parse().ok()?;
    match spec.as_bytes()[0] {
        b'+' => Some(StackIndex::FromTop(n)),
        b'-' => Some(StackIndex::FromBottom(n)),
        _ => None,
    }
}

fn resolve_stack_index(index: StackIndex, len: usize) -> Option<usize> {
    match index {
        StackIndex::FromTop(n) => (n < len).then_some(n),
        StackIndex::FromBottom(n) => len.checked_sub(n + 1),
    }
}

/// `~` for `$HOME`, as `dirs` shows directories unless given `-l`.
fn abbreviate_home(dir: &str, state: &ShellState) -> String {
    match typed_home(state).filter(|home| !home.is_empty() && home != "/") {
        Some(home) if dir == home => "~".to_string(),
        Some(home) => match dir.strip_prefix(home.as_str()) {
            Some(rest) if rest.starts_with('/') => format!("~{rest}"),
            _ => dir.to_string(),
        },
        None => dir.to_string(),
    }
}

fn print_stack(state: &ShellState) {
    let entries: Vec<String> = stack_entries(state)
        .iter()
        .map(|dir| abbreviate_home(dir, state))
        .collect();
    println!("{}", entries.join(" "));
}

/// Parse `[-n] [+N | -N | dir]` for pushd/popd: returns (no_cd, operand).
fn stack_command_args<'a>(name: &str, args: &'a [String]) -> Result<(bool, Option<&'a str>), i32> {
    let mut no_change = false;
    let mut operand = None;
    for arg in args {
        if arg == "-n" {
            no_change = true;
        } else if arg == "--" {
            continue;
        } else if operand.is_none() {
            operand = Some(arg.as_str());
        } else {
            eprintln!("jsh: {name}: too many arguments");
            return Err(1);
        }
    }
    Ok((no_change, operand))
}

fn builtin_pushd(args: &[String], state: &mut ShellState) -> i32 {
    let (no_change, operand) = match stack_command_args("pushd", args) {
        Ok(parsed) => parsed,
        Err(status) => return status,
    };
    let mut entries = stack_entries(state);
    let physical = physical_by_default(state);
    match operand {
        // No operand: exchange the top two entries.
        None => {
            if entries.len() < 2 {
                eprintln!("jsh: pushd: no other directory");
                return 1;
            }
            entries.swap(0, 1);
            if !no_change {
                if let Err(error) = enter_directory(&entries[0], physical, state) {
                    eprintln!("jsh: pushd: {}: {}", entries[0], os_error_text(&error));
                    return 1;
                }
                entries[0] = logical_cwd(state);
            }
            store_stack(state, &entries);
        }
        // `+N`/`-N`: rotate the stack so that entry becomes the top.
        Some(spec) if stack_index(spec).is_some() => {
            let Some(position) =
                stack_index(spec).and_then(|index| resolve_stack_index(index, entries.len()))
            else {
                eprintln!("jsh: pushd: {spec}: directory stack index out of range");
                return 1;
            };
            entries.rotate_left(position);
            if !no_change {
                if let Err(error) = enter_directory(&entries[0], physical, state) {
                    eprintln!("jsh: pushd: {}: {}", entries[0], os_error_text(&error));
                    return 1;
                }
                entries[0] = logical_cwd(state);
            }
            store_stack(state, &entries);
        }
        Some(dir) => {
            if no_change {
                // -n DIR adds DIR below the current directory.
                entries.insert(1, dir.to_string());
            } else {
                match enter_directory(dir, physical, state) {
                    Ok(new) => entries.insert(0, new),
                    Err(error) => {
                        eprintln!("jsh: pushd: {dir}: {}", os_error_text(&error));
                        return 1;
                    }
                }
            }
            store_stack(state, &entries);
        }
    }
    print_stack(state);
    0
}

fn builtin_popd(args: &[String], state: &mut ShellState) -> i32 {
    let (no_change, operand) = match stack_command_args("popd", args) {
        Ok(parsed) => parsed,
        Err(status) => return status,
    };
    let mut entries = stack_entries(state);
    if entries.len() < 2 {
        eprintln!("jsh: popd: directory stack empty");
        return 1;
    }
    let position = match operand {
        // Plain popd removes the top and changes to the new top; with -n it
        // removes the entry below the top instead.
        None => usize::from(no_change),
        Some(spec) => {
            match stack_index(spec).and_then(|index| resolve_stack_index(index, entries.len())) {
                Some(position) => position,
                None if stack_index(spec).is_some() => {
                    eprintln!("jsh: popd: {spec}: directory stack index out of range");
                    return 1;
                }
                None => {
                    eprintln!("jsh: popd: {spec}: invalid argument");
                    eprintln!("popd: usage: popd [-n] [+N | -N]");
                    return 2;
                }
            }
        }
    };
    entries.remove(position);
    if position == 0 && !no_change {
        let physical = physical_by_default(state);
        if let Err(error) = enter_directory(&entries[0], physical, state) {
            eprintln!("jsh: popd: {}: {}", entries[0], os_error_text(&error));
            return 1;
        }
        entries[0] = logical_cwd(state);
    }
    store_stack(state, &entries);
    print_stack(state);
    0
}

fn builtin_dirs(args: &[String], state: &mut ShellState) -> i32 {
    let mut long = false;
    let mut per_line = false;
    let mut numbered = false;
    let mut select: Option<&str> = None;
    for arg in args {
        if stack_index(arg).is_some() {
            select = Some(arg);
            continue;
        }
        let Some(flags) = arg.strip_prefix('-').filter(|flags| !flags.is_empty()) else {
            eprintln!("jsh: dirs: {arg}: invalid argument");
            eprintln!("dirs: usage: dirs [-clpv] [+N] [-N]");
            return 2;
        };
        for flag in flags.chars() {
            match flag {
                // `dirs -c` clears the stack and prints nothing.
                'c' => {
                    state.dir_stack.clear();
                    return 0;
                }
                'l' => long = true,
                'p' => per_line = true,
                'v' => {
                    per_line = true;
                    numbered = true;
                }
                _ => {
                    eprintln!("jsh: dirs: -{flag}: invalid option");
                    eprintln!("dirs: usage: dirs [-clpv] [+N] [-N]");
                    return 2;
                }
            }
        }
    }
    let entries: Vec<String> = stack_entries(state)
        .into_iter()
        .map(|dir| {
            if long {
                dir
            } else {
                abbreviate_home(&dir, state)
            }
        })
        .collect();
    if let Some(spec) = select {
        return match stack_index(spec).and_then(|index| resolve_stack_index(index, entries.len())) {
            Some(position) => {
                println!("{}", entries[position]);
                0
            }
            None => {
                eprintln!(
                    "jsh: dirs: {}: directory stack index out of range",
                    &spec[1..]
                );
                1
            }
        };
    }
    if numbered {
        for (position, dir) in entries.iter().enumerate() {
            println!("{position:2}  {dir}");
        }
    } else if per_line {
        for dir in &entries {
            println!("{dir}");
        }
    } else {
        println!("{}", entries.join(" "));
    }
    0
}

// ============================================================
// [[ ]] with real regex support (Phase 2)
// ============================================================

fn builtin_double_bracket(args: &[String], state: &mut ShellState) -> i32 {
    let args: Vec<&str> = args
        .iter()
        .map(|s| s.as_str())
        .filter(|s| *s != "]]")
        .collect();
    if args.is_empty() {
        return 1;
    }
    let mut cond = Conditional {
        args: &args,
        pos: 0,
        state,
    };
    let status = cond.or(true);
    if cond.pos < args.len() && status != 2 {
        eprintln!("jsh: [[: syntax error near `{}'", args[cond.pos]);
        return 2;
    }
    status
}

/// Evaluator for the expanded operands of `[[ ]]`: 0 true, 1 false, 2 error.
///
/// `&&` and `||` short-circuit as in bash: the skipped side is still parsed
/// (so the position stays right) but not evaluated, because evaluating can
/// assign (`[[ x -eq y++ ]]`) or fail (`[[ -n $x || 1/0 -eq 0 ]]`).
struct Conditional<'a, 'b> {
    args: &'a [&'a str],
    pos: usize,
    state: &'b mut ShellState,
}

impl Conditional<'_, '_> {
    fn or(&mut self, evaluate: bool) -> i32 {
        let mut result = self.and(evaluate);
        while self.args.get(self.pos) == Some(&"||") {
            self.pos += 1;
            let right = self.and(evaluate && result == 1);
            if evaluate && result == 1 {
                result = right;
            }
        }
        result
    }

    fn and(&mut self, evaluate: bool) -> i32 {
        let mut result = self.primary(evaluate);
        while self.args.get(self.pos) == Some(&"&&") {
            self.pos += 1;
            let right = self.primary(evaluate && result == 0);
            if evaluate && result == 0 {
                result = right;
            }
        }
        result
    }

    fn primary(&mut self, evaluate: bool) -> i32 {
        let args = self.args;
        let Some(&word) = args.get(self.pos) else {
            return 1;
        };
        if word == "!" && self.pos + 1 < args.len() {
            self.pos += 1;
            return match self.primary(evaluate) {
                0 => 1,
                1 => 0,
                error => error,
            };
        }
        if word == "(" {
            self.pos += 1;
            let result = self.or(evaluate);
            if args.get(self.pos) == Some(&")") {
                self.pos += 1;
            }
            return result;
        }
        // A binary operator in the middle wins over reading the left operand
        // as a unary operator: in `[[ $1 == -v ]]` with `$1` = `-n`, the `-n`
        // is an operand. The operators reach us as expanded strings, so this is
        // the order that keeps option-like values working.
        if self.pos + 2 < args.len() && is_cond_binary_op(args[self.pos + 1]) {
            let (left, op, right) = (word, args[self.pos + 1], args[self.pos + 2]);
            self.pos += 3;
            return if evaluate {
                self.binary(left, op, right)
            } else {
                0
            };
        }
        if self.pos + 1 < args.len() && is_unary_test_op(word) {
            let operand = args[self.pos + 1];
            self.pos += 2;
            // Skipped operands are not evaluated: `-v a[i++]` has effects.
            return if !evaluate || unary_test(word, operand, self.state).unwrap_or(false) {
                0
            } else {
                1
            };
        }
        self.pos += 1;
        if word.is_empty() {
            1
        } else {
            0
        }
    }

    fn binary(&mut self, left: &str, op: &str, right: &str) -> i32 {
        let status = |value: bool| if value { 0 } else { 1 };
        match op {
            // The expander turned quoted characters of the right operand into
            // literal bracket expressions, so it is always a pattern here.
            "==" | "=" => status(cond_pattern_match(right, left, self.state)),
            "!=" => status(!cond_pattern_match(right, left, self.state)),
            "<" => status(left < right),
            ">" => status(left > right),
            "=~" => eval_regex_match(left, right, self.state),
            "-eq" | "-ne" | "-lt" | "-le" | "-gt" | "-ge" => {
                match (self.arithmetic(left), self.arithmetic(right)) {
                    (Some(a), Some(b)) => status(compare_integers(op, a, b)),
                    _ => 1,
                }
            }
            _ => status(binary_file_test(op, left, right).unwrap_or(false)),
        }
    }

    /// Inside `[[ ]]` the operands of `-eq` and friends are arithmetic
    /// expressions: `[[ 1+1 -eq 2 ]]` is true and `010` is octal. A malformed
    /// expression is reported and makes the test false, as in bash.
    fn arithmetic(&mut self, expr: &str) -> Option<i64> {
        let saved_error = self.state.expansion_error.take();
        let value = crate::expand::expand_arithmetic(expr, self.state);
        let error = std::mem::replace(&mut self.state.expansion_error, saved_error);
        match error {
            Some(message) => {
                eprintln!("jsh: [[: {message}");
                None
            }
            None => value.parse().ok(),
        }
    }
}

fn is_unary_test_op(op: &str) -> bool {
    matches!(
        op,
        "-n" | "-z"
            | "-a"
            | "-e"
            | "-f"
            | "-d"
            | "-b"
            | "-c"
            | "-p"
            | "-S"
            | "-h"
            | "-L"
            | "-s"
            | "-g"
            | "-u"
            | "-k"
            | "-r"
            | "-w"
            | "-x"
            | "-O"
            | "-G"
            | "-N"
            | "-t"
            | "-o"
            | "-v"
            | "-R"
    )
}

fn is_cond_binary_op(op: &str) -> bool {
    matches!(
        op,
        "==" | "="
            | "!="
            | "<"
            | ">"
            | "-eq"
            | "-ne"
            | "-lt"
            | "-le"
            | "-gt"
            | "-ge"
            | "=~"
            | "-nt"
            | "-ot"
            | "-ef"
    )
}

/// `[[ left =~ right ]]`. Quoted parts of the operand were already escaped by
/// the expander, so `right` is always a regex here.
///
/// BASH_REMATCH is populated with the whole match followed by the capture
/// groups; `=~` is close to useless without it.
fn eval_regex_match(left: &str, right: &str, state: &mut ShellState) -> i32 {
    let built = regex::RegexBuilder::new(right)
        .case_insensitive(crate::expand::nocasematch(state))
        .build();
    match built {
        Ok(re) => {
            if let Some(captures) = re.captures(left) {
                let mut rematch = Vec::new();
                for i in 0..captures.len() {
                    rematch.push(
                        captures
                            .get(i)
                            .map(|m| m.as_str().to_string())
                            .unwrap_or_default(),
                    );
                }
                state.set_array("BASH_REMATCH", rematch);
                0
            } else {
                state.set_array("BASH_REMATCH", Vec::new());
                1
            }
        }
        Err(e) => {
            // Loud, like bash: a bad regex is a usage error, not "no match".
            eprintln!("jsh: [[: {}: invalid regex: {}", right, e);
            2
        }
    }
}

/// The right operand of `[[ x == pat ]]`. Bash always treats it as a pattern;
/// matching literally when it holds no metacharacter is just the cheap path to
/// the same answer. `[` counts as one ; `[[ a == [ab] ]]` is true ; and so do
/// the extended-glob openers once `shopt -s extglob` is on.
fn cond_pattern_match(pattern: &str, text: &str, state: &ShellState) -> bool {
    // `shopt -s nocasematch` folds case for `[[ == ]]` as it does for `case`.
    if crate::expand::nocasematch(state) {
        let (pattern, text) = (pattern.to_lowercase(), text.to_lowercase());
        return cond_pattern_match_exact(&pattern, &text, state);
    }
    cond_pattern_match_exact(pattern, text, state)
}

fn cond_pattern_match_exact(pattern: &str, text: &str, state: &ShellState) -> bool {
    let extglob = state.shell_opts.extglob;
    let is_pattern = pattern.contains(['*', '?', '['])
        || (extglob && crate::glob_match::contains_extglob(pattern));
    if is_pattern {
        crate::glob_match::pattern_match(pattern, text, extglob)
    } else {
        pattern == text
    }
}

// ============================================================
// declare (Phase 1)
// ============================================================

fn builtin_declare(args: &[String], state: &mut ShellState) -> i32 {
    crate::var_builtins::declare(args, state)
}

// ============================================================
// z-jump (Phase 5)
// ============================================================

fn builtin_z(args: &[String], state: &mut ShellState) -> i32 {
    let z_db = crate::zjump::get_z_db();

    // Handle list/remove/clear operations with the lock held
    {
        let mut z_db = z_db.lock().unwrap_or_else(|e| e.into_inner());

        if args.is_empty() || (args.len() == 1 && args[0] == "-l") {
            for (path, score) in z_db.list() {
                println!("{:>10.1}  {}", score, path);
            }
            return 0;
        }

        if args.len() == 2 && args[0] == "-x" {
            return match z_db.try_remove(&args[1]) {
                Ok(()) => 0,
                Err(error) => {
                    eprintln!("jsh: z: could not persist removal: {error}");
                    1
                }
            };
        }

        if args.len() == 1 && args[0] == "-c" {
            let result = env::current_dir().and_then(|cwd| z_db.try_remove(&cwd.to_string_lossy()));
            return match result {
                Ok(()) => 0,
                Err(error) => {
                    eprintln!("jsh: z: could not persist removal: {error}");
                    1
                }
            };
        }
    }

    // Query and cd: drop the lock before calling update_directory_vars
    // (which also acquires z_db lock)
    let keywords: Vec<&str> = args.iter().map(|s| s.as_str()).collect();
    let target = {
        let z_db = z_db.lock().unwrap_or_else(|e| e.into_inner());
        z_db.query(&keywords)
    };

    match target {
        Some(target) => {
            let old_dir = env::current_dir().ok();
            match env::set_current_dir(&target) {
                Ok(()) => {
                    println!("{}", target);
                    if let Ok(new_dir) = env::current_dir() {
                        update_directory_vars(old_dir.as_deref(), &new_dir, state);
                    }
                    0
                }
                Err(e) => {
                    eprintln!("jsh: z: {}: {}", target, e);
                    1
                }
            }
        }
        None => {
            eprintln!("jsh: z: no match for: {}", args.join(" "));
            1
        }
    }
}

// ============================================================
// hook (Phase 4)
// ============================================================

fn builtin_hook(args: &[String], state: &mut ShellState) -> i32 {
    if args.is_empty() || args[0] == "list" {
        println!("precmd:  {:?}", state.hooks.precmd);
        println!("preexec: {:?}", state.hooks.preexec);
        println!("chpwd:   {:?}", state.hooks.chpwd);
        return 0;
    }

    if args.len() < 3 {
        eprintln!("Usage: hook add|remove precmd|preexec|chpwd <function>");
        return 1;
    }

    let action = &args[0];
    let hook_type = &args[1];
    let func = &args[2];

    let hook_list = match hook_type.as_str() {
        "precmd" => &mut state.hooks.precmd,
        "preexec" => &mut state.hooks.preexec,
        "chpwd" => &mut state.hooks.chpwd,
        _ => {
            eprintln!("jsh: hook: unknown hook type: {}", hook_type);
            return 1;
        }
    };

    match action.as_str() {
        "add" => {
            if !hook_list.contains(func) {
                hook_list.push(func.clone());
            }
        }
        "remove" => {
            hook_list.retain(|h| h != func);
        }
        _ => {
            eprintln!("jsh: hook: unknown action: {} (use add or remove)", action);
            return 1;
        }
    }
    0
}

// ============================================================
// complete / compgen (Phase 7)
// ============================================================

/// The action each short `complete` flag stands for, as bash defines them.
fn short_flag_action(flag: &str) -> &'static str {
    match flag {
        "-a" => "alias",
        "-b" => "builtin",
        "-c" => "command",
        "-e" => "export",
        "-g" => "group",
        "-j" => "job",
        "-k" => "keyword",
        "-s" => "service",
        "-u" => "user",
        "-v" => "variable",
        _ => "",
    }
}

fn builtin_complete(args: &[String], state: &mut ShellState) -> i32 {
    if args.is_empty() {
        // List all completion specs
        for (cmd, spec) in &state.completion_specs {
            let mut parts = Vec::new();
            if let Some(ref wl) = spec.word_list {
                parts.push(format!("-W \"{}\"", wl.join(" ")));
            }
            if let Some(ref f) = spec.function {
                parts.push(format!("-F {}", f));
            }
            if spec.directory {
                parts.push("-d".to_string());
            }
            if spec.file {
                parts.push("-f".to_string());
            }
            println!("complete {} {}", parts.join(" "), cmd);
        }
        return 0;
    }

    // Parse flags
    let mut word_list: Option<Vec<String>> = None;
    let mut function: Option<String> = None;
    let mut directory = false;
    let mut file = false;
    let mut glob_pattern: Option<String> = None;
    let mut filter_pattern: Option<String> = None;
    let mut prefix: Option<String> = None;
    let mut suffix: Option<String> = None;
    let mut actions: Vec<String> = Vec::new();
    let mut remove = false;
    let mut fallback_spec = false;
    let mut command_names: Vec<String> = Vec::new();
    let mut i = 0;

    while i < args.len() {
        match args[i].as_str() {
            // `-o` names a behaviour flag jsh does not act on, and `-C`
            // names a command to run for completions, which this shell will
            // not do on a keystroke. Both still consume their argument:
            // `complete -o default -F __nvm nvm` must not register a spec
            // for the word `default`.
            "-o" | "-C" => i += 1,
            // `-A <action>` names one of the sources this shell already has.
            "-A" => {
                i += 1;
                if let Some(action) = args.get(i) {
                    actions.push(action.clone());
                }
            }
            // `-D`, `-E` and `-I` name no command: they install the fallback
            // completion, the empty-line completion and the first-word one.
            // jsh has nowhere to put them yet, but they are valid calls and
            // must not be reported as a missing command name.
            "-D" | "-E" | "-I" => fallback_spec = true,
            // The short spellings of the same actions, as bash defines them.
            "-a" | "-b" | "-c" | "-e" | "-g" | "-j" | "-k" | "-s" | "-u" | "-v" => {
                actions.push(short_flag_action(&args[i]).to_string());
            }
            // `-p` prints specs rather than naming an action.
            "-p" => {}
            // Everything after `--` is a command name, empty string included.
            "--" => {
                command_names.extend(args[i + 1..].iter().cloned());
                i = args.len();
            }
            "-W" => {
                i += 1;
                if i < args.len() {
                    word_list = Some(args[i].split_whitespace().map(|s| s.to_string()).collect());
                }
            }
            "-F" => {
                i += 1;
                if i < args.len() {
                    function = Some(args[i].clone());
                }
            }
            "-d" => directory = true,
            "-f" => file = true,
            "-G" => {
                i += 1;
                if i < args.len() {
                    glob_pattern = Some(args[i].clone());
                }
            }
            "-X" => {
                i += 1;
                if i < args.len() {
                    filter_pattern = Some(args[i].clone());
                }
            }
            "-P" => {
                i += 1;
                if i < args.len() {
                    prefix = Some(args[i].clone());
                }
            }
            "-S" => {
                i += 1;
                if i < args.len() {
                    suffix = Some(args[i].clone());
                }
            }
            "-r" => remove = true,
            name => command_names.push(name.to_string()),
        }
        i += 1;
    }

    // One `complete` call names any number of commands ; bash-completion
    // registers `_longopt` for two dozen at a time ; and `''` is one of them,
    // the spec bash uses when the command word is empty.
    if command_names.is_empty() {
        if remove {
            state.completion_specs.clear();
            return 0;
        }
        if fallback_spec {
            return 0;
        }
        eprintln!("jsh: complete: no command specified");
        return 1;
    }

    for command_name in command_names {
        if remove {
            state.completion_specs.remove(&command_name);
            continue;
        }
        state.completion_specs.insert(
            command_name.clone(),
            crate::environment::CompletionSpec {
                command: command_name,
                word_list: word_list.clone(),
                function: function.clone(),
                directory,
                file,
                actions: actions.clone(),
                glob_pattern: glob_pattern.clone(),
                filter_pattern: filter_pattern.clone(),
                prefix: prefix.clone(),
                suffix: suffix.clone(),
            },
        );
    }
    0
}

/// Bash's reserved words, in the order `compgen -k` lists them.
const BASH_KEYWORDS: &[&str] = &[
    "if", "then", "else", "elif", "fi", "case", "esac", "for", "select", "while", "until", "do",
    "done", "in", "function", "time", "{", "}", "!", "[[", "]]", "coproc",
];

fn builtin_compgen(args: &[String], state: &mut ShellState) -> i32 {
    if args.is_empty() {
        return 0;
    }
    let mut word_list: Vec<String> = Vec::new();
    let mut action: Option<&str> = None;
    let mut prefix = "";
    let mut glob_pattern: Option<&str> = None;
    let mut i = 0;

    while i < args.len() {
        match args[i].as_str() {
            "-W" => {
                i += 1;
                if i < args.len() {
                    word_list = args[i].split_whitespace().map(|s| s.to_string()).collect();
                }
            }
            "-A" => {
                i += 1;
                if i < args.len() {
                    action = Some(args[i].as_str());
                }
            }
            "-d" => action = Some("directory"),
            "-f" => action = Some("file"),
            flag @ ("-a" | "-b" | "-c" | "-e" | "-g" | "-j" | "-k" | "-s" | "-u" | "-v") => {
                action = Some(short_flag_action(flag));
            }
            "-G" => {
                i += 1;
                if i < args.len() {
                    glob_pattern = Some(args[i].as_str());
                }
            }
            s if !s.starts_with('-') => {
                prefix = s;
            }
            _ => {}
        }
        i += 1;
    }

    let mut results: Vec<String> = Vec::new();

    // One implementation of what each action names, shared with the Tab
    // completer and with `complete -A`: `compgen -A user` and a `complete -A
    // user` spec must not be able to disagree about what a user is. Bash lists
    // keywords in its own table order and everything else sorted.
    if let Some(action) = action {
        if action == "keyword" {
            results.extend(
                BASH_KEYWORDS
                    .iter()
                    .filter(|keyword| keyword.starts_with(prefix))
                    .map(|keyword| keyword.to_string()),
            );
        } else {
            let mut found: Vec<String> = crate::completer::action_candidates(action, prefix, state)
                .into_iter()
                .map(|completion| completion.text)
                .collect();
            found.sort();
            found.dedup();
            results.extend(found);
        }
    }

    if let Some(pat) = glob_pattern {
        if let Ok(paths) = glob::glob(pat) {
            for path in paths.flatten() {
                if let Some(s) = path.to_str() {
                    if prefix.is_empty() || s.starts_with(prefix) {
                        results.push(s.to_string());
                    }
                }
            }
        }
    }

    // `-W` words keep their order (and repeats), as in bash.
    for word in &word_list {
        if word.starts_with(prefix) {
            results.push(word.clone());
        }
    }

    for r in &results {
        println!("{}", r);
    }
    if results.is_empty() {
        1
    } else {
        0
    }
}

// ============================================================
// disown (Phase 8)
// ============================================================

fn builtin_disown(args: &[String], state: &mut ShellState) -> i32 {
    if args.is_empty() || args[0] == "-a" {
        // Disown all or last
        if args.first().map(|s| s.as_str()) == Some("-a") {
            state.jobs.jobs.clear();
        } else if let Some(job) = state.jobs.get_last() {
            let id = job.id;
            state.jobs.jobs.retain(|j| j.id != id);
        } else {
            eprintln!("jsh: disown: no current job");
            return 1;
        }
        return 0;
    }

    let mut last_status = 0;
    for arg in args {
        match state.jobs.resolve_spec(arg) {
            Some(id) => state.jobs.jobs.retain(|j| j.id != id),
            None => {
                eprintln!("jsh: disown: {arg}: no such job");
                last_status = 1;
            }
        }
    }
    last_status
}

/// How many reaped background statuses `wait` remembers, so that waiting
/// again for a PID that already finished still reports its status (bash keeps
/// a similar bounded list).
const MAX_REMEMBERED_STATUSES: usize = 1024;

fn remember_status(state: &mut ShellState, pid: i32, status: i32) {
    let remembered = &mut state.builtin_state.reaped;
    remembered.retain(|(known, _)| *known != pid);
    if remembered.len() == MAX_REMEMBERED_STATUSES {
        remembered.pop_front();
    }
    remembered.push_back((pid, status));
}

/// Shell-style status of a finished child, or `None` if it has not finished.
fn exit_status(status: nix::sys::wait::WaitStatus) -> Option<(i32, i32)> {
    use nix::sys::wait::WaitStatus;
    match status {
        WaitStatus::Exited(pid, code) => Some((pid.as_raw(), code)),
        WaitStatus::Signaled(pid, signal, _) => Some((pid.as_raw(), 128 + signal as i32)),
        _ => None,
    }
}

/// Block until `pid` finishes and return its status. A job the shell already
/// reaped (an interactive shell polls its background jobs) answers from the
/// job table or from the remembered statuses.
fn wait_for_pid(pid: i32, state: &mut ShellState) -> Result<i32, i32> {
    if let Some(job) = state.jobs.jobs.iter().find(|job| job.pid.as_raw() == pid) {
        if let crate::job::JobStatus::Done(code) = job.status {
            state.jobs.jobs.retain(|job| job.pid.as_raw() != pid);
            remember_status(state, pid, code);
            return Ok(code);
        }
    }
    loop {
        match interruptible_wait(pid)? {
            Ok(status) => {
                if let Some((_, code)) = exit_status(status) {
                    state.jobs.jobs.retain(|job| job.pid.as_raw() != pid);
                    remember_status(state, pid, code);
                    return Ok(code);
                }
            }
            Err(nix::errno::Errno::EINTR) => {
                if let Some(status) = crate::signal::pending_status() {
                    return Err(status);
                }
            }
            Err(_) => {
                return match state
                    .builtin_state
                    .reaped
                    .iter()
                    .find(|(known, _)| *known == pid)
                {
                    Some(&(_, code)) => Ok(code),
                    None => Err(127),
                };
            }
        }
    }
}

/// One blocking waitpid that a trapped signal interrupts, as bash's `wait`
/// is: the builtin then returns 128+N and the trap runs at the next safe
/// point. A latched termination unwinds with its own status.
fn interruptible_wait(pid: i32) -> Result<nix::Result<nix::sys::wait::WaitStatus>, i32> {
    match crate::signal::wait_interruptibly(nix::unistd::Pid::from_raw(pid)) {
        crate::signal::WaitOutcome::Done(waited) => Ok(waited),
        crate::signal::WaitOutcome::Trapped(signal) => Err(128 + signal),
        crate::signal::WaitOutcome::Terminating(status) => Err(status),
    }
}

/// `wait -n`: the status of the next job (among `pids`, or any job) to finish.
fn wait_for_next(pids: Option<Vec<i32>>, state: &mut ShellState) -> Result<(i32, i32), i32> {
    let candidates: Vec<i32> = match pids {
        Some(pids) => pids,
        None => state.jobs.jobs.iter().map(|job| job.pid.as_raw()).collect(),
    };
    if candidates.is_empty() {
        return Err(127);
    }
    // A job that an interactive shell already reaped counts as finished.
    if let Some(job) = state.jobs.jobs.iter().find(|job| {
        candidates.contains(&job.pid.as_raw())
            && matches!(job.status, crate::job::JobStatus::Done(_))
    }) {
        let pid = job.pid.as_raw();
        let crate::job::JobStatus::Done(code) = job.status else {
            unreachable!()
        };
        state.jobs.jobs.retain(|job| job.pid.as_raw() != pid);
        remember_status(state, pid, code);
        return Ok((pid, code));
    }
    loop {
        match interruptible_wait(-1)? {
            Ok(status) => {
                let Some((pid, code)) = exit_status(status) else {
                    continue;
                };
                state.jobs.jobs.retain(|job| job.pid.as_raw() != pid);
                remember_status(state, pid, code);
                if candidates.contains(&pid) {
                    return Ok((pid, code));
                }
            }
            Err(nix::errno::Errno::EINTR) => {
                if let Some(status) = crate::signal::pending_status() {
                    return Err(status);
                }
            }
            Err(_) => return Err(127),
        }
    }
}

fn builtin_wait(args: &[String], state: &mut ShellState) -> i32 {
    let mut next = false;
    let mut pid_variable: Option<String> = None;
    let mut index = 0;
    while let Some(arg) = args.get(index) {
        if arg == "--" {
            index += 1;
            break;
        }
        let Some(flags) = arg.strip_prefix('-').filter(|flags| !flags.is_empty()) else {
            break;
        };
        // A negative number is not an option list.
        if flags.bytes().all(|b| b.is_ascii_digit()) {
            break;
        }
        index += 1;
        for flag in flags.chars() {
            match flag {
                'n' => next = true,
                // Waiting until termination is what jsh always does.
                'f' => {}
                'p' => match args.get(index) {
                    Some(name) => {
                        pid_variable = Some(name.clone());
                        index += 1;
                    }
                    None => {
                        eprintln!("jsh: wait: -p: option requires an argument");
                        return 2;
                    }
                },
                _ => {
                    eprintln!("jsh: wait: -{flag}: invalid option");
                    eprintln!("wait: usage: wait [-fn] [-p var] [id ...]");
                    return 2;
                }
            }
        }
    }
    if let Some(name) = &pid_variable {
        if !crate::environment::is_valid_identifier(name) {
            eprintln!("jsh: wait: `{name}': not a valid identifier");
            return 2;
        }
        state.unset_var(name);
    }

    // Resolve every operand to a PID first, reporting bad ones as bash does.
    let mut pids = Vec::new();
    let mut status = 0;
    for arg in &args[index..] {
        if arg.starts_with('%') {
            match state
                .jobs
                .resolve_spec(arg)
                .and_then(|id| state.jobs.get_by_id(id).map(|job| job.pid.as_raw()))
            {
                Some(pid) => pids.push(pid),
                None => {
                    eprintln!("jsh: wait: {arg}: no such job");
                    status = 127;
                }
            }
        } else {
            match arg.parse::<i32>() {
                Ok(pid) if pid > 0 => pids.push(pid),
                _ => {
                    eprintln!("jsh: wait: `{arg}': not a pid or valid job spec");
                    status = 1;
                }
            }
        }
    }

    if next {
        let operands = (index < args.len()).then_some(pids);
        if operands.as_ref().is_some_and(Vec::is_empty) {
            return status;
        }
        return match wait_for_next(operands, state) {
            Ok((pid, code)) => {
                if let Some(name) = &pid_variable {
                    state.set_var(name, &pid.to_string());
                }
                code
            }
            Err(code) => code,
        };
    }

    if index == args.len() {
        // `wait` alone waits for every job and returns 0.
        let jobs: Vec<i32> = state.jobs.jobs.iter().map(|job| job.pid.as_raw()).collect();
        for pid in jobs {
            if let Err(code) = wait_for_pid(pid, state) {
                if code != 127 {
                    return code;
                }
            }
        }
        return 0;
    }

    for pid in pids {
        status = match wait_for_pid(pid, state) {
            Ok(code) => code,
            Err(127) => {
                eprintln!("jsh: wait: pid {pid} is not a child of this shell");
                127
            }
            Err(code) => return code,
        };
        if let Some(name) = &pid_variable {
            state.set_var(name, &pid.to_string());
        }
    }
    status
}

// ============================================================
// shopt (shell options)
// ============================================================

/// Every `shopt` name bash knows, in the order bash lists them, paired with the
/// value a fresh bash reports for it.
///
/// jsh enforces the globbing and navigation options; the rest are remembered in
/// `shell_opts.shopt_opts` so a `.bashrc` that opens with `shopt -s histappend`
/// starts a shell instead of an error, and `shopt histappend` still answers
/// with what was set. Rejecting them would be worse than remembering them: the
/// stock Debian `.bashrc` alone sets two, and bash-completion sets two more.
pub(crate) const SHOPT_OPTIONS: &[(&str, bool)] = &[
    ("autocd", false),
    ("assoc_expand_once", false),
    ("cdable_vars", false),
    ("cdspell", false),
    ("checkhash", false),
    ("checkjobs", false),
    ("checkwinsize", true),
    ("cmdhist", true),
    ("compat31", false),
    ("compat32", false),
    ("compat40", false),
    ("compat41", false),
    ("compat42", false),
    ("compat43", false),
    ("compat44", false),
    ("complete_fullquote", true),
    ("direxpand", false),
    ("dirspell", false),
    ("dotglob", false),
    ("execfail", false),
    ("expand_aliases", true),
    ("extdebug", false),
    ("extglob", false),
    ("extquote", true),
    ("failglob", false),
    ("force_fignore", true),
    ("globasciiranges", true),
    ("globskipdots", true),
    ("globstar", false),
    ("gnu_errfmt", false),
    ("histappend", false),
    ("histreedit", false),
    ("histverify", false),
    ("hostcomplete", true),
    ("huponexit", false),
    ("inherit_errexit", false),
    ("interactive_comments", true),
    ("lastpipe", false),
    ("lithist", false),
    ("localvar_inherit", false),
    ("localvar_unset", false),
    ("login_shell", false),
    ("mailwarn", false),
    ("no_empty_cmd_completion", false),
    ("nocaseglob", false),
    ("nocasematch", false),
    ("noexpand_translation", false),
    ("noglob", false), // jsh extension: bash spells this `set -f`
    ("nullglob", false),
    ("patsub_replacement", true),
    ("progcomp", true),
    ("progcomp_alias", false),
    ("promptvars", true),
    ("restricted_shell", false),
    ("shift_verbose", false),
    ("sourcepath", true),
    ("varredir_close", false),
    ("xpg_echo", false),
];

const SHOPT_USAGE: &str = "shopt: usage: shopt [-pqsu] [-o] [optname ...]";

pub fn shopt_option_is_known(name: &str) -> bool {
    SHOPT_OPTIONS.iter().any(|(n, _)| *n == name)
}

/// The live value of one `shopt` name: the field jsh keeps for it, else what
/// was last remembered, else bash's default.
fn shopt_enabled(state: &ShellState, name: &str) -> bool {
    match name {
        "dotglob" => state.shell_opts.dotglob,
        "nullglob" => state.shell_opts.nullglob,
        "failglob" => state.shell_opts.failglob,
        "extglob" => state.shell_opts.extglob,
        "nocaseglob" => state.shell_opts.nocaseglob,
        "noglob" => state.shell_opts.noglob,
        "globstar" => state.shell_opts.globstar,
        "lastpipe" => state.shell_opts.lastpipe,
        "autocd" => state.shell_opts.autocd,
        "cdspell" => state.shell_opts.cdspell,
        "checkwinsize" => state.shell_opts.checkwinsize,
        "inherit_errexit" => state.shell_opts.inherit_errexit,
        other => match state.shell_opts.shopt_opts.get(other) {
            Some(&enabled) => enabled,
            None => SHOPT_OPTIONS
                .iter()
                .find(|(n, _)| *n == other)
                .map(|(_, default)| *default)
                .unwrap_or(false),
        },
    }
}

/// Apply one `shopt -s NAME` / `shopt -u NAME`.
pub fn set_shopt_option(state: &mut ShellState, name: &str, enable: bool) {
    match name {
        "dotglob" => state.shell_opts.dotglob = enable,
        "nullglob" => state.shell_opts.nullglob = enable,
        "failglob" => state.shell_opts.failglob = enable,
        "extglob" => state.shell_opts.extglob = enable,
        "nocaseglob" => state.shell_opts.nocaseglob = enable,
        "noglob" => state.shell_opts.noglob = enable,
        "globstar" => state.shell_opts.globstar = enable,
        "lastpipe" => state.shell_opts.lastpipe = enable,
        "autocd" => state.shell_opts.autocd = enable,
        "cdspell" => state.shell_opts.cdspell = enable,
        "checkwinsize" => state.shell_opts.checkwinsize = enable,
        "inherit_errexit" => state.shell_opts.inherit_errexit = enable,
        other => {
            state
                .shell_opts
                .shopt_opts
                .insert(other.to_string(), enable);
        }
    }
}

fn builtin_shopt(args: &[String], state: &mut ShellState) -> i32 {
    // shopt [-pqsu] [-o] [optname ...]
    // -s/-u set or unset, -p prints a line that can be read back, -q prints
    // nothing and answers through the exit status, -o works on `set -o` names.
    let mut setting: Option<bool> = None;
    let mut reusable = false;
    let mut quiet = false;
    let mut set_o = false;

    let mut first_name = args.len();
    for (i, arg) in args.iter().enumerate() {
        if arg == "--" {
            first_name = i + 1;
            break;
        }
        // Only a `-` cluster is an option; the first bare word starts the names.
        if !(arg.len() > 1 && arg.starts_with('-')) {
            first_name = i;
            break;
        }
        for c in arg.chars().skip(1) {
            match c {
                's' => setting = Some(true),
                'u' => setting = Some(false),
                'p' => reusable = true,
                'q' => quiet = true,
                'o' => set_o = true,
                _ => {
                    eprintln!("jsh: shopt: -{}: invalid option", c);
                    eprintln!("jsh: {}", SHOPT_USAGE);
                    return 2;
                }
            }
        }
        first_name = i + 1;
    }
    let names = &args[first_name..];

    if names.is_empty() {
        if !quiet {
            print_shopt_options(state, setting, reusable, set_o);
        }
        return 0;
    }

    let mut exit_code = 0;
    // Query mode reports failure unless *every* name is on, so it has to look at
    // all of them; `-s`/`-u` still apply the names that are valid.
    let mut all_enabled = true;
    for name in names {
        let known = if set_o {
            set_option_is_known(name)
        } else {
            shopt_option_is_known(name)
        };
        if !known {
            eprintln!("jsh: shopt: {}: invalid shell option name", name);
            exit_code = 1;
            all_enabled = false;
            continue;
        }
        match setting {
            Some(enable) => {
                if set_o {
                    apply_shell_option(state, name, enable);
                } else {
                    set_shopt_option(state, name, enable);
                }
            }
            None => {
                let enabled = if set_o {
                    shell_option_enabled(state, name)
                } else {
                    shopt_enabled(state, name)
                };
                all_enabled &= enabled;
                if !quiet {
                    print_shopt_line(name, enabled, reusable, set_o);
                }
            }
        }
    }

    if setting.is_none() && !all_enabled && exit_code == 0 {
        exit_code = 1;
    }
    exit_code
}

fn print_shopt_line(name: &str, enabled: bool, reusable: bool, set_o: bool) {
    match (reusable, set_o) {
        (true, true) => println!("set {}o {}", if enabled { '-' } else { '+' }, name),
        (true, false) => println!("shopt -{} {}", if enabled { 's' } else { 'u' }, name),
        // Bash pads the name to 15 columns, then a tab.
        (false, _) => println!("{:<15}\t{}", name, if enabled { "on" } else { "off" }),
    }
}

/// `shopt` with no names lists options: all of them, or ; after `-s` / `-u` ;
/// only those currently on or off.
fn print_shopt_options(state: &ShellState, setting: Option<bool>, reusable: bool, set_o: bool) {
    if set_o {
        for (name, _) in SET_OPTIONS {
            let enabled = shell_option_enabled(state, name);
            if setting.is_none_or(|want| want == enabled) {
                print_shopt_line(name, enabled, reusable, true);
            }
        }
        return;
    }
    for (name, _) in SHOPT_OPTIONS {
        let enabled = shopt_enabled(state, name);
        if setting.is_none_or(|want| want == enabled) {
            print_shopt_line(name, enabled, reusable, false);
        }
    }
}

// ============================================================
// Structured data pipeline builtins (Phase 9)
// ============================================================

fn builtin_from_json() -> i32 {
    let records = crate::structured::read_json_stdin();
    crate::structured::write_json_stdout(&records);
    0
}

fn builtin_to_json() -> i32 {
    // Read JSON from stdin and re-serialize (identity, but normalizes)
    let records = crate::structured::read_json_stdin();
    crate::structured::write_json_stdout(&records);
    0
}

fn builtin_to_table() -> i32 {
    let records = crate::structured::read_json_stdin();
    let table = crate::structured::to_table(&records);
    print!("{}", table);
    0
}

fn builtin_where(args: &[String]) -> i32 {
    if args.len() < 3 {
        eprintln!("Usage: where <field> <op> <value>");
        return 1;
    }
    let field = &args[0];
    let op = &args[1];
    let value = &args[2];
    let records = crate::structured::read_json_stdin();
    let filtered = crate::structured::filter_where(&records, field, op, value);
    crate::structured::write_json_stdout(&filtered);
    0
}

fn builtin_sort_by(args: &[String]) -> i32 {
    if args.is_empty() {
        eprintln!("Usage: sort-by <field> [-r]");
        return 1;
    }
    let field = &args[0];
    let reverse = args.get(1).map(|s| s == "-r").unwrap_or(false);
    let mut records = crate::structured::read_json_stdin();
    crate::structured::sort_by(&mut records, field, reverse);
    crate::structured::write_json_stdout(&records);
    0
}

fn builtin_select(args: &[String]) -> i32 {
    if args.is_empty() {
        eprintln!("Usage: select <field1> [field2] ...");
        return 1;
    }
    let fields: Vec<&str> = args.iter().map(|s| s.as_str()).collect();
    let records = crate::structured::read_json_stdin();
    let projected = crate::structured::select_fields(&records, &fields);
    crate::structured::write_json_stdout(&projected);
    0
}

// ============================================================
// bookmark (Feature 10)
// ============================================================

fn builtin_bookmark(args: &[String], state: &mut ShellState) -> i32 {
    if args.is_empty() {
        eprintln!("Usage: bookmark <add|go|ls|rm> [args...]");
        return 1;
    }
    match args[0].as_str() {
        "add" => {
            let name = match args.get(1) {
                Some(n) => n.clone(),
                None => {
                    eprintln!("Usage: bookmark add <name> [path]");
                    return 1;
                }
            };
            let path = args.get(2).cloned().unwrap_or_else(|| {
                std::env::current_dir()
                    .map(|p| p.to_string_lossy().to_string())
                    .unwrap_or_else(|_| ".".to_string())
            });
            if let Ok(mut db) = crate::bookmarks::get_bookmark_db().lock() {
                if !db.add(&name, &path) {
                    return 1;
                }
                println!(
                    "Bookmark '{}' -> {}",
                    crate::terminal_text::escape_inline(&name, 16 * 1024),
                    crate::terminal_text::escape_inline(&path, 16 * 1024)
                );
            }
            0
        }
        "go" => {
            let name = match args.get(1) {
                Some(n) => n,
                None => {
                    eprintln!("Usage: bookmark go <name>");
                    return 1;
                }
            };
            let path = {
                if let Ok(db) = crate::bookmarks::get_bookmark_db().lock() {
                    db.get(name).cloned()
                } else {
                    None
                }
            };
            match path {
                Some(path) => {
                    let old_dir = std::env::current_dir().ok();
                    if let Err(e) = std::env::set_current_dir(&path) {
                        eprintln!("jsh: bookmark go: {}: {}", path, e);
                        return 1;
                    }
                    if let Ok(new_dir) = std::env::current_dir() {
                        update_directory_vars(old_dir.as_deref(), &new_dir, state);
                    }
                    0
                }
                None => {
                    eprintln!("jsh: bookmark '{}' not found", name);
                    1
                }
            }
        }
        "ls" => {
            if let Ok(db) = crate::bookmarks::get_bookmark_db().lock() {
                for (name, path) in db.list() {
                    println!("  {:<16} {}", name, path);
                }
            }
            0
        }
        "rm" => {
            let name = match args.get(1) {
                Some(n) => n,
                None => {
                    eprintln!("Usage: bookmark rm <name>");
                    return 1;
                }
            };
            if let Ok(mut db) = crate::bookmarks::get_bookmark_db().lock() {
                match db.try_remove(name) {
                    Ok(true) => {
                        println!("Removed bookmark '{}'", name);
                        0
                    }
                    Ok(false) => {
                        eprintln!("jsh: bookmark '{}' not found", name);
                        1
                    }
                    Err(error) => {
                        eprintln!("jsh: bookmark: could not persist removal: {error}");
                        1
                    }
                }
            } else {
                1
            }
        }
        _ => {
            eprintln!("Usage: bookmark <add|go|ls|rm> [args...]");
            1
        }
    }
}

fn builtin_workflow(args: &[String], state: &ShellState) -> i32 {
    const USAGE: &str = "Usage: workflow <list|show|render> [name] [parameter=value ...]";

    let (subcommand, rest) = match args.split_first() {
        None => ("list", &[][..]),
        Some((subcommand, rest)) => (subcommand.as_str(), rest),
    };
    match subcommand {
        "-h" | "--help" | "help" => {
            println!("{USAGE}");
            println!("  list [--json]                 List available workflows");
            println!("  show NAME [--json]            Inspect one workflow template");
            println!("  render NAME parameter=value;  Render without executing");
            0
        }
        "list" => {
            if rest == ["--json"] {
                match serde_json::to_string_pretty(&state.workflow_registry.all()) {
                    Ok(json) => println!("{json}"),
                    Err(error) => {
                        eprintln!("jsh: workflow: cannot render registry: {error}");
                        return 1;
                    }
                }
            } else if rest.is_empty() {
                for workflow in state.workflow_registry.all() {
                    println!("{:<28} {}", workflow.name, workflow.description);
                }
            } else {
                eprintln!("jsh: workflow: list accepts only --json");
                return 2;
            }
            0
        }
        "show" => {
            let Some(name) = rest.first() else {
                eprintln!("jsh: workflow: show requires a name");
                return 2;
            };
            let json = rest.get(1).is_some_and(|argument| argument == "--json");
            if rest.len() > if json { 2 } else { 1 } {
                eprintln!("jsh: workflow: unexpected argument to show");
                return 2;
            }
            let Some(workflow) = state.workflow_registry.get(name) else {
                eprintln!("jsh: workflow: unknown workflow '{name}'");
                return 1;
            };
            if json {
                match serde_json::to_string_pretty(workflow) {
                    Ok(json) => println!("{json}"),
                    Err(error) => {
                        eprintln!("jsh: workflow: cannot render '{name}': {error}");
                        return 1;
                    }
                }
            } else {
                println!("{} ; {}", workflow.name, workflow.description);
                println!("  {}", workflow.command);
                if !workflow.parameters.is_empty() {
                    println!("Parameters:");
                    for parameter in &workflow.parameters {
                        let requirement = parameter
                            .default
                            .as_deref()
                            .map(|value| format!("default: {value}"))
                            .unwrap_or_else(|| "required".to_string());
                        let description = parameter.description.as_deref().unwrap_or("");
                        println!("  {:16} {:20} {}", parameter.name, requirement, description);
                    }
                }
            }
            0
        }
        "render" => {
            let Some(name) = rest.first() else {
                eprintln!("jsh: workflow: render requires a name");
                return 2;
            };
            let Some(workflow) = state.workflow_registry.get(name) else {
                eprintln!("jsh: workflow: unknown workflow '{name}'");
                return 1;
            };
            let mut values: Vec<(String, String)> = workflow
                .parameters
                .iter()
                .filter_map(|parameter| {
                    parameter
                        .default
                        .as_ref()
                        .map(|value| (parameter.name.clone(), value.clone()))
                })
                .collect();
            let mut assigned = std::collections::HashSet::new();
            for assignment in &rest[1..] {
                let Some((key, value)) = assignment.split_once('=') else {
                    eprintln!("jsh: workflow: expected parameter=value, got '{assignment}'");
                    return 2;
                };
                if !workflow
                    .parameters
                    .iter()
                    .any(|parameter| parameter.name == key)
                {
                    eprintln!("jsh: workflow: '{name}' has no parameter '{key}'");
                    return 2;
                }
                if !assigned.insert(key) {
                    eprintln!("jsh: workflow: duplicate value for parameter '{key}'");
                    return 2;
                }
                if let Some(existing) = values.iter_mut().find(|(parameter, _)| parameter == key) {
                    existing.1 = value.to_string();
                } else {
                    values.push((key.to_string(), value.to_string()));
                }
            }
            for parameter in &workflow.parameters {
                if !values.iter().any(|(name, _)| name == &parameter.name) {
                    eprintln!(
                        "jsh: workflow: missing value for '{}' (pass {}=value)",
                        parameter.name, parameter.name
                    );
                    return 2;
                }
            }
            match crate::workflows::fill_template(&workflow.command, &values) {
                Ok(command) => {
                    println!("{command}");
                    0
                }
                Err(error) => {
                    eprintln!("jsh: workflow: cannot render '{name}': {error}");
                    1
                }
            }
        }
        // A workflow name by itself is a convenient read-only shorthand for
        // `show`; Ctrl-G remains the parameter-filling interface.
        name if rest.is_empty() && state.workflow_registry.get(name).is_some() => {
            builtin_workflow(&["show".to_string(), name.to_string()], state)
        }
        _ => {
            eprintln!("jsh: workflow: unknown subcommand or workflow '{subcommand}'");
            eprintln!("{USAGE}");
            2
        }
    }
}

// ============================================================
// Enhanced structured data pipeline builtins (Feature 13)
// ============================================================

fn builtin_from_csv() -> i32 {
    let records = crate::structured::read_csv_stdin();
    crate::structured::write_json_stdout(&records);
    0
}

fn builtin_group_by(args: &[String]) -> i32 {
    if args.is_empty() {
        eprintln!("Usage: group-by <field>");
        return 1;
    }
    let records = crate::structured::read_json_stdin();
    let grouped = crate::structured::group_by(&records, &args[0]);
    let out = serde_json::to_string_pretty(&grouped).unwrap_or_default();
    println!("{}", out);
    0
}

fn builtin_unique(args: &[String]) -> i32 {
    let field = args.first().map(|s| s.as_str());
    let records = crate::structured::read_json_stdin();
    let unique = crate::structured::unique(&records, field);
    crate::structured::write_json_stdout(&unique);
    0
}

fn builtin_count() -> i32 {
    let records = crate::structured::read_json_stdin();
    println!("{}", crate::structured::count(&records));
    0
}

fn builtin_math(args: &[String]) -> i32 {
    if args.len() < 2 {
        eprintln!("Usage: math <sum|avg|min|max> <field>");
        return 1;
    }
    let op = &args[0];
    let field = &args[1];
    let records = crate::structured::read_json_stdin();
    match crate::structured::math_op(&records, op, field) {
        Some(result) => {
            println!("{}", result);
            0
        }
        None => {
            eprintln!("math: no numeric values for field '{}'", field);
            1
        }
    }
}

fn builtin_help(args: &[String], state: &ShellState) -> i32 {
    if args.is_empty() {
        println!("jsh ; a Bash-inspired shell with structured data pipelines\n");
        println!("Commands:");
        for command in crate::command_catalog::entries() {
            println!("  {:18} {}", command.name, command.summary());
        }
        if !state.user_signatures.is_empty() {
            let mut user: Vec<&str> = state.user_signatures.keys().map(|s| s.as_str()).collect();
            user.sort_unstable();
            println!("\nUser-defined functions:");
            for chunk in user.chunks(6) {
                println!("  {}", chunk.join("  "));
            }
        }
        println!("\nType 'help <command>' for details on a specific command.");
        return 0;
    }

    // -r / --record asks for the signature as a JSON record on stdout.
    let mut as_record = false;
    let mut cmd: Option<&str> = None;
    for a in args {
        match a.as_str() {
            "-r" | "--record" => as_record = true,
            other => {
                if cmd.is_none() {
                    cmd = Some(other);
                }
            }
        }
    }
    let cmd = match cmd {
        Some(c) => c,
        None => {
            return builtin_help(&[], state);
        }
    };

    // Phase 15c: user-defined signatures take precedence so re-defs are visible.
    if let Some(rsig) = state.user_signatures.get(cmd) {
        if as_record {
            match serde_json::to_string_pretty(&rsig.to_record().to_json()) {
                Ok(rendered) => println!("{}", rendered),
                Err(error) => {
                    eprintln!("jsh: help: failed to render help: {}", error);
                    return 1;
                }
            }
        } else {
            print!("{}", rsig.render_help());
        }
        return 0;
    }

    if let Some(command) = crate::command_catalog::get(cmd) {
        if as_record {
            let json = command.help_record().to_json();
            match serde_json::to_string_pretty(&json) {
                Ok(rendered) => println!("{}", rendered),
                Err(error) => {
                    eprintln!("jsh: help: failed to render help: {}", error);
                    return 1;
                }
            }
        } else if let Some(signature) = command.signature() {
            print!("{}", signature.render_help());
            if command.name != command.canonical_name {
                println!("Alias: {} -> {}", command.name, command.canonical_name);
            }
        } else {
            let alias = if command.name != command.canonical_name {
                format!(" (alias of {})", command.canonical_name)
            } else {
                String::new()
            };
            println!("{}{}\n  {}", command.name, alias, command.summary());
            if let Some(usage) = command.usage() {
                println!("\nUsage: {usage}");
            }
            if let Some(detail) = command.detail() {
                println!("\n{detail}");
            }
        }
        if !as_record {
            if let Some(note) = command.shadow_note() {
                println!("\n{note}");
            }
        }
        return 0;
    }
    eprintln!("jsh: help: no help for '{}'", cmd);
    1
}
