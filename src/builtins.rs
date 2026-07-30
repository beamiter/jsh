/// Built-in shell commands.
use crate::environment::ShellState;
use crate::parser;
use std::env;
use std::io::IsTerminal;
use std::path::Path;
use std::sync::atomic::{AtomicBool, AtomicI32, Ordering};

/// Set by the `exit` builtin so the main loop can exit gracefully
/// (allowing session save, history save, EXIT trap to run).
pub static EXIT_REQUESTED: AtomicBool = AtomicBool::new(false);
pub static EXIT_CODE: AtomicI32 = AtomicI32::new(0);

pub fn reset_exit_request() {
    EXIT_CODE.store(0, Ordering::SeqCst);
    EXIT_REQUESTED.store(false, Ordering::SeqCst);
}

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
    "break",
    "continue",
    "shift",
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
    "from-csv",
    "group-by",
    "unique",
    "count",
    "math",
    "exec",
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
    BUILTIN_NAMES.contains(&name) || crate::value_builtins::is_value_aware(name)
}

pub fn run_builtin(name: &str, args: &[String], state: &mut ShellState) -> i32 {
    match name {
        "agent" => builtin_agent(args, state),
        "cd" => builtin_cd(args, state),
        "exit" => builtin_exit(args, state),
        "export" => builtin_export(args, state),
        "unset" => builtin_unset(args, state),
        "echo" => builtin_echo(args),
        "printf" => builtin_printf(args),
        "pwd" => builtin_pwd(),
        "alias" => builtin_alias(args, state),
        "unalias" => builtin_unalias(args, state),
        "type" => builtin_type(args, state),
        "source" | "." => builtin_source(args, state),
        "eval" => builtin_eval(args, state),
        "read" => builtin_read(args, state),
        ":" => 0,
        "true" => 0,
        "false" => 1,
        "test" | "[" => builtin_test(args),
        "set" => builtin_set(args, state),
        "local" => builtin_local(args, state),
        "return" => builtin_return(args, state),
        "break" => builtin_loop_control("break", args, state),
        "continue" => builtin_loop_control("continue", args, state),
        "shift" => builtin_shift(args, state),
        "exec" => builtin_exec(args, state),
        "help" => builtin_help(args, state),
        "history" => builtin_history(state),
        "context" => crate::execution_context::run_args(args),
        "pushd" => builtin_pushd(args, state),
        "popd" => builtin_popd(state),
        "dirs" => builtin_dirs(state),
        "trap" => builtin_trap(args, state),
        "jobs" => {
            state.jobs.print_jobs();
            0
        }
        "fg" => {
            let id = args
                .first()
                .and_then(|s| s.trim_start_matches('%').parse().ok());
            match id {
                Some(id) => state.jobs.continue_fg(id),
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
            }
        }
        "bg" => {
            let id = args
                .first()
                .and_then(|s| s.trim_start_matches('%').parse().ok());
            match id {
                Some(id) => state.jobs.continue_bg(id),
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
            }
        }
        "[[" => builtin_double_bracket(args, state),
        "command" => {
            // Strip the option prefix: -v/-V describe the command instead of
            // running it, -p only affects which PATH is searched.
            let mut describe: Option<char> = None;
            let mut rest = args;
            while let Some(first) = rest.first() {
                match first.as_str() {
                    "-v" | "-V" => {
                        describe = Some(first.chars().nth(1).unwrap());
                        rest = &rest[1..];
                    }
                    "-p" => rest = &rest[1..],
                    "--" => {
                        rest = &rest[1..];
                        break;
                    }
                    _ => break,
                }
            }
            if let Some(mode) = describe {
                return command_describe(rest, mode == 'V', state);
            }
            let args = rest;
            if args.is_empty() {
                return 0;
            }
            let cmd_name = &args[0];
            if is_builtin(cmd_name) {
                run_builtin(cmd_name, &args[1..], state)
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
            if is_builtin(cmd_name) {
                run_builtin(cmd_name, &args[1..], state)
            } else {
                eprintln!("jsh: builtin: {}: not a shell builtin", cmd_name);
                1
            }
        }
        "hash" => 0,
        // New builtins
        "declare" => builtin_declare(args, state),
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
        // Stream processing commands
        "sum" => crate::stream::builtin_sum(args),
        "avg" => crate::stream::builtin_avg(args),
        "min" => crate::stream::builtin_min(args),
        "max" => crate::stream::builtin_max(args),
        // `lines` is value-aware (Phase 6b) — falls through to `_` adapter.
        "stats" => crate::stream::builtin_stats(args),
        "trim" => crate::stream::builtin_trim(args),
        // `reverse` is value-aware (Phase 5a) — fall through to adapter.
        "upper" => crate::stream::builtin_upper(args),
        "lower" => crate::stream::builtin_lower(args),
        // Debug commands
        "debug-trace" => crate::debug::builtin_debug_trace(args),
        "debug-timing" => crate::debug::builtin_debug_timing(args),
        "debug-profile" => crate::debug::builtin_debug_profile(args),
        // Data processing commands
        "filter" => crate::data::builtin_filter(args),
        "map" => crate::data::builtin_map(args),
        // `group-by` and `select` are value-aware in Phase 5a — fall through.
        "uniq" => crate::data::builtin_uniq(args),
        // `shuffle` is value-aware (Phase 10c) — fall through to adapter.
        "dedupe" => crate::data::builtin_dedupe(args),
        _ => {
            // Phase 5a: fork-path adapter for value-aware builtins.
            // Reads stdin as bytes, runs the value-aware fn, writes JSON bytes
            // to stdout. Used when a value-aware builtin runs inside a forked
            // pipeline child (mixed with non-value-aware commands).
            if let Some(vfn) = crate::value_builtins::VALUE_BUILTINS.get(name) {
                return run_value_builtin_in_fork(*vfn, args, state);
            }
            eprintln!("jsh: {}: builtin not yet implemented", name);
            1
        }
    }
}

fn run_value_builtin_in_fork(
    vfn: crate::value_builtins::ValueBuiltin,
    args: &[String],
    state: &mut ShellState,
) -> i32 {
    use crate::pipeline_data::PipelineData;
    use std::io::{Read, Write};
    let mut buf = Vec::new();
    if !std::io::stdin().is_terminal() {
        let _ = std::io::stdin().lock().read_to_end(&mut buf);
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
            let out = match out {
                PipelineData::Stream(it) => PipelineData::Values(it.collect()),
                other => other,
            };
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
        Err(c) => c,
    }
}

// ============================================================
// Original builtins
// ============================================================

fn builtin_cd(args: &[String], state: &mut ShellState) -> i32 {
    let target = if args.is_empty() {
        state.home_dir.to_string_lossy().to_string()
    } else if args[0] == "-" {
        match state.get_var("OLDPWD") {
            Some(d) => {
                println!("{}", d);
                d.to_string()
            }
            None => {
                eprintln!("jsh: cd: OLDPWD not set");
                return 1;
            }
        }
    } else if args[0].starts_with('+') || args[0].starts_with('-') {
        // Handle directory stack navigation: cd +N or cd -N
        if let Ok(idx) = args[0][1..].parse::<usize>() {
            if args[0].starts_with('+') {
                if idx < state.dir_stack.len() {
                    state.dir_stack[idx].to_string_lossy().to_string()
                } else {
                    eprintln!("jsh: cd: invalid stack index: +{}", idx);
                    return 1;
                }
            } else {
                // -N means from the end
                if idx > 0 && idx <= state.dir_stack.len() {
                    state.dir_stack[state.dir_stack.len() - idx]
                        .to_string_lossy()
                        .to_string()
                } else {
                    eprintln!("jsh: cd: invalid stack index: -{}", idx);
                    return 1;
                }
            }
        } else {
            args[0].clone()
        }
    } else {
        args[0].clone()
    };

    let old_dir = env::current_dir().ok();

    // Try to change to target directory
    // First try as absolute/relative path
    if let Ok(new_dir) = change_to_directory(&target, state) {
        update_directory_vars(old_dir.as_deref(), &new_dir, state);
        return 0;
    }

    // Try CDPATH if target doesn't contain /
    if !target.contains('/') {
        if let Some(cdpath_ref) = state.get_var("CDPATH") {
            let cdpath = cdpath_ref.to_string();
            for dir in cdpath.split(':') {
                if dir.is_empty() {
                    continue;
                }
                let candidate = format!("{}/{}", dir, target);
                if let Ok(new_dir) = change_to_directory(&candidate, state) {
                    println!("{}", new_dir.display());
                    update_directory_vars(old_dir.as_deref(), &new_dir, state);
                    return 0;
                }
            }
        }
    }

    eprintln!("jsh: cd: {}: No such file or directory", target);
    1
}

fn change_to_directory(
    path: &str,
    _state: &mut ShellState,
) -> Result<std::path::PathBuf, std::io::Error> {
    let old = env::current_dir().ok();
    env::set_current_dir(path)?;

    match env::current_dir() {
        Ok(new_dir) => Ok(new_dir),
        Err(e) => {
            if let Some(old_dir) = old {
                let _ = env::set_current_dir(&old_dir);
            }
            Err(e)
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

    // z-jump: record directory visit
    if let Ok(mut z_db) = crate::zjump::get_z_db().lock() {
        z_db.add(&new_dir.to_string_lossy());
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
    code
}

fn builtin_loop_control(name: &str, _args: &[String], state: &mut ShellState) -> i32 {
    if state.loop_depth == 0 {
        eprintln!("jsh: {}: only meaningful in a loop", name);
        return 1;
    }

    if name == "break" {
        state.loop_break = true;
    } else {
        state.loop_continue = true;
    }
    0
}

fn builtin_export(args: &[String], state: &mut ShellState) -> i32 {
    if args.is_empty() {
        let mut vars: Vec<_> = state.env_vars.iter().collect();
        vars.sort_by_key(|(k, _)| (*k).clone());
        for (k, v) in vars {
            println!("declare -x {}=\"{}\"", k, v);
        }
        return 0;
    }

    if args.first().map(|s| s.as_str()) == Some("-n") {
        for arg in &args[1..] {
            if let Some(val) = state.env_vars.remove(arg) {
                env::remove_var(arg);
                // If in function scope, set to local_vars; otherwise just unset
                if let Some(scope) = state.local_vars_stack.last_mut() {
                    scope.insert(arg.clone(), val);
                }
            }
        }
        return 0;
    }

    for arg in args {
        if let Some(eq_pos) = arg.find('=') {
            let name = &arg[..eq_pos];
            let value = &arg[eq_pos + 1..];
            state.export_var(name, value);
        } else {
            // Get value from any scope
            if let Some(val) = state.get_var(arg).map(|s| s.to_string()) {
                state.export_var(arg, &val);
            } else if !state.env_vars.contains_key(arg) {
                state.export_var(arg, "");
            }
        }
    }
    0
}

fn builtin_unset(args: &[String], state: &mut ShellState) -> i32 {
    for name in args {
        if name == "-v" || name == "-f" {
            continue;
        }
        // Support unset arr[idx]
        if let Some(bracket) = name.find('[') {
            if name.ends_with(']') {
                let var_name = &name[..bracket];
                let idx = &name[bracket + 1..name.len() - 1];
                if let Some(arr) = state.arrays.get_mut(var_name) {
                    if let Ok(i) = idx.parse::<usize>() {
                        if i < arr.len() {
                            arr[i] = String::new();
                        }
                    }
                } else if let Some(map) = state.assoc_arrays.get_mut(var_name) {
                    map.remove(idx);
                }
                continue;
            }
        }
        state.unset_var(name);
    }
    0
}

fn builtin_echo(args: &[String]) -> i32 {
    let mut newline = true;
    let mut interpret_escapes = false;
    let mut start = 0;

    for (i, arg) in args.iter().enumerate() {
        match arg.as_str() {
            "-n" => {
                newline = false;
                start = i + 1;
            }
            "-e" => {
                interpret_escapes = true;
                start = i + 1;
            }
            "-E" => {
                interpret_escapes = false;
                start = i + 1;
            }
            "-ne" | "-en" => {
                newline = false;
                interpret_escapes = true;
                start = i + 1;
            }
            _ => break,
        }
    }

    let text = args[start..].join(" ");
    if interpret_escapes {
        print!("{}", unescape_echo(&text));
    } else {
        print!("{}", text);
    }
    if newline {
        println!();
    }
    0
}

fn unescape_echo(s: &str) -> String {
    let mut result = String::new();
    let mut chars = s.chars();
    while let Some(c) = chars.next() {
        if c == '\\' {
            match chars.next() {
                Some('n') => result.push('\n'),
                Some('t') => result.push('\t'),
                Some('r') => result.push('\r'),
                Some('\\') => result.push('\\'),
                Some('a') => result.push('\x07'),
                Some('b') => result.push('\x08'),
                Some('0') => result.push('\0'),
                Some(c2) => {
                    result.push('\\');
                    result.push(c2);
                }
                None => result.push('\\'),
            }
        } else {
            result.push(c);
        }
    }
    result
}

fn builtin_pwd() -> i32 {
    match env::current_dir() {
        Ok(p) => {
            println!("{}", p.display());
            0
        }
        Err(e) => {
            eprintln!("jsh: pwd: {}", e);
            1
        }
    }
}

fn builtin_alias(args: &[String], state: &mut ShellState) -> i32 {
    if args.is_empty() {
        for (k, v) in &state.aliases {
            println!("alias {}='{}'", k, v);
        }
        return 0;
    }
    for arg in args {
        if let Some(eq_pos) = arg.find('=') {
            let name = &arg[..eq_pos];
            let value = &arg[eq_pos + 1..];
            let value = value.trim_matches('\'').trim_matches('"');
            state.aliases.insert(name.to_string(), value.to_string());
        } else {
            match state.aliases.get(arg) {
                Some(v) => println!("alias {}='{}'", arg, v),
                None => {
                    eprintln!("jsh: alias: {}: not found", arg);
                    return 1;
                }
            }
        }
    }
    0
}

fn builtin_unalias(args: &[String], state: &mut ShellState) -> i32 {
    for name in args {
        if name == "-a" {
            state.aliases.clear();
            return 0;
        }
        state.aliases.remove(name);
    }
    0
}

fn builtin_type(args: &[String], state: &mut ShellState) -> i32 {
    let mut ret = 0;
    for arg in args {
        if is_builtin(arg) {
            println!("{} is a shell builtin", arg);
        } else if state.aliases.contains_key(arg) {
            println!("{} is aliased to '{}'", arg, state.aliases[arg]);
        } else if state.functions.contains_key(arg) {
            println!("{} is a function", arg);
        } else if let Some(path) = find_in_path(arg) {
            println!("{} is {}", arg, path);
        } else {
            eprintln!("jsh: type: {}: not found", arg);
            ret = 1;
        }
    }
    ret
}

fn find_in_path(cmd: &str) -> Option<String> {
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
    )
}

/// `command -v NAME...` / `command -V NAME...`: report how each name would be
/// resolved, without running it. Exit status is 1 if any name is not found.
fn command_describe(names: &[String], verbose: bool, state: &ShellState) -> i32 {
    let mut ret = 0;
    for name in names {
        // (terse form for -v, description for -V) of the first match, in the
        // order the shell itself resolves a command name.
        let found = if let Some(alias) = state.aliases.get(name) {
            Some((
                format!("alias {}='{}'", name, alias),
                format!("{} is aliased to `{}'", name, alias),
            ))
        } else if is_shell_keyword(name) {
            Some((name.clone(), format!("{} is a shell keyword", name)))
        } else if state.functions.contains_key(name) {
            Some((name.clone(), format!("{} is a function", name)))
        } else if is_builtin(name) {
            Some((name.clone(), format!("{} is a shell builtin", name)))
        } else {
            find_in_path(name).map(|path| (path.clone(), format!("{} is {}", name, path)))
        };

        match found {
            Some((terse, described)) => println!("{}", if verbose { described } else { terse }),
            None => {
                if verbose {
                    eprintln!("jsh: command: {}: not found", name);
                }
                ret = 1;
            }
        }
    }
    ret
}

/// Use bash to source a script file when jsh's parser can't handle it,
/// then reload environment variables and simple functions back into jsh.
fn source_via_bash(path: &str, source_args: &[String], state: &mut ShellState) -> i32 {
    // Create a bash script that sources the file and outputs environment variables
    let bash_script = r#"
set -a
source -- "$1" "${@:2}"
set +a

# Output all environment variables in key=value format
declare -p | grep 'declare -x' | sed 's/declare -x //' | sed "s/='/'=/g"

# Output alias definitions for later parsing if needed
alias 2>/dev/null || true

# Output function names
declare -F | awk '{print $3}'
"#;

    // Execute bash script to capture the environment
    let mut command = std::process::Command::new("bash");
    command
        .arg("-c")
        .arg(bash_script)
        .arg("jsh-source")
        .arg(path)
        .args(source_args);
    match command.output() {
        Ok(output) => {
            let stdout = String::from_utf8_lossy(&output.stdout);
            let stderr = String::from_utf8_lossy(&output.stderr);

            // If bash had errors, print them but continue
            if !stderr.is_empty() && !stderr.contains("warning") {
                eprintln!("jsh: bash source warnings: {}", stderr.trim());
            }

            // Parse exported variables from bash output
            for line in stdout.lines() {
                // Skip function names (no = sign) and aliases
                if line.contains('=') && !line.starts_with("alias") {
                    if let Some(eq_pos) = line.find('=') {
                        let key = &line[..eq_pos];
                        let value = &line[eq_pos + 1..];
                        // Remove quotes if present
                        let value = if (value.starts_with('\'') && value.ends_with('\''))
                            || (value.starts_with('"') && value.ends_with('"'))
                        {
                            &value[1..value.len() - 1]
                        } else {
                            value
                        };
                        state.export_var(key, value);
                    }
                }
            }

            // Return success (bash exit code is usually 0 for sourcing)
            if output.status.success() {
                0
            } else {
                1
            }
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
    // so push a frame for the duration of the source.
    let old_bash_source = state.arrays.get("BASH_SOURCE").cloned();
    let mut frames = old_bash_source.clone().unwrap_or_default();
    frames.insert(0, resolved_path.clone());
    state.set_array("BASH_SOURCE", frames);

    state.return_depth += 1;
    let result = match std::fs::read_to_string(&resolved_path) {
        Ok(content) => {
            match parser::parse(&content) {
                Ok(commands) => {
                    // Parse succeeded, execute all commands in current shell context
                    let last = crate::executor::execute_program(&commands, state);
                    // `return` exits a sourced file but must not leak into the
                    // caller's command list.
                    if state.return_requested {
                        state.return_requested = false;
                    }
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
    match old_bash_source {
        Some(frames) => state.set_array("BASH_SOURCE", frames),
        None => {
            state.arrays.remove("BASH_SOURCE");
        }
    }

    result
}

fn builtin_eval(args: &[String], state: &mut ShellState) -> i32 {
    if args.is_empty() {
        return 0;
    }

    // Join all arguments with space, just like bash does
    let input = args.join(" ");

    // Parse and execute the input
    match parser::parse(&input) {
        Ok(commands) => {
            let mut last = 0;
            for cmd in &commands {
                last = crate::executor::execute_complete_command(cmd, state);
                // Early return doesn't stop eval loop (unlike source)
                if state.loop_break || state.loop_continue {
                    break;
                }
            }
            last
        }
        Err(e) => {
            eprintln!("jsh: eval: parse error: {}", e);
            2
        }
    }
}

fn builtin_read(args: &[String], state: &mut ShellState) -> i32 {
    let mut prompt_str = None;
    let mut silent = false;
    let mut raw = false;
    let mut _timeout_secs: Option<f64> = None;
    let mut count_chars: Option<usize> = None;
    let mut delim = '\n';
    let mut exact_count: Option<usize> = None;
    let mut read_array = false;
    let mut var_names: Vec<&str> = Vec::new();
    let mut i = 0;

    while i < args.len() {
        match args[i].as_str() {
            "-p" => {
                i += 1;
                if i < args.len() {
                    prompt_str = Some(args[i].as_str());
                }
            }
            "-s" => silent = true,
            "-r" => raw = true,
            "-t" => {
                i += 1;
                if i < args.len() {
                    _timeout_secs = args[i].parse::<f64>().ok();
                }
            }
            "-n" => {
                i += 1;
                if i < args.len() {
                    count_chars = args[i].parse::<usize>().ok();
                }
            }
            "-N" => {
                i += 1;
                if i < args.len() {
                    exact_count = args[i].parse::<usize>().ok();
                }
            }
            "-d" => {
                i += 1;
                if i < args.len() {
                    let d = &args[i];
                    if !d.is_empty() {
                        delim = d.chars().next().unwrap();
                    }
                }
            }
            "-a" => {
                read_array = true;
            }
            s if s.starts_with('-') => {}
            _ => {
                var_names.push(&args[i]);
            }
        }
        i += 1;
    }

    if var_names.is_empty() && !read_array {
        var_names.push("REPLY");
    }

    if let Some(p) = prompt_str {
        eprint!("{}", p);
        use std::io::Write;
        std::io::stderr().flush().ok();
    }

    if silent {
        std::process::Command::new("stty")
            .arg("-echo")
            .status()
            .ok();
    }

    let result = if let Some(count) = exact_count {
        // Read exactly N characters
        read_exact_chars(count, &var_names, read_array, state)
    } else if let Some(count) = count_chars {
        // Read up to N characters
        read_limited_chars(count, delim, &var_names, read_array, state)
    } else {
        // Read line with delimiter
        read_line_with_delimiter(delim, raw, &var_names, read_array, state)
    };

    if silent {
        std::process::Command::new("stty").arg("echo").status().ok();
        eprintln!();
    }

    result
}

fn read_exact_chars(
    count: usize,
    var_names: &[&str],
    read_array: bool,
    state: &mut ShellState,
) -> i32 {
    let mut buffer = vec![0u8; count];
    let mut filled = 0;
    let mut stdin = std::io::stdin().lock();
    while filled < buffer.len() {
        match read_interruptibly(&mut stdin, &mut buffer[filled..]) {
            Ok(0) => return 1,
            Ok(read) => filled += read,
            Err(status) => return status,
        }
    }

    let line = String::from_utf8_lossy(&buffer).into_owned();
    if read_array {
        let parts: Vec<&str> = line.split_whitespace().collect();
        if let Some(arr_name) = var_names.first() {
            state.arrays.insert(
                arr_name.to_string(),
                parts.into_iter().map(|s| s.to_string()).collect(),
            );
        }
    } else if var_names.len() == 1 {
        state.set_var(var_names[0], &line);
    }
    0
}

fn read_interruptibly(reader: &mut impl std::io::Read, buffer: &mut [u8]) -> Result<usize, i32> {
    loop {
        if let Some(status) = crate::signal::pending_status() {
            return Err(status);
        }
        match reader.read(buffer) {
            Ok(read) => return Ok(read),
            Err(error) if error.kind() == std::io::ErrorKind::Interrupted => {
                if let Some(status) = crate::signal::pending_status() {
                    return Err(status);
                }
            }
            Err(_) => return Err(1),
        }
    }
}

fn read_limited_chars(
    max_count: usize,
    delim: char,
    var_names: &[&str],
    read_array: bool,
    state: &mut ShellState,
) -> i32 {
    let mut buffer = vec![0u8; max_count];
    let mut stdin = std::io::stdin().lock();
    match read_interruptibly(&mut stdin, &mut buffer) {
        Ok(n) if n > 0 => {
            buffer.truncate(n);
            let line = String::from_utf8_lossy(&buffer).into_owned();
            if read_array {
                let parts: Vec<&str> = line.split(delim).collect();
                if let Some(arr_name) = var_names.first() {
                    state.arrays.insert(
                        arr_name.to_string(),
                        parts.into_iter().map(|s| s.to_string()).collect(),
                    );
                }
            } else if var_names.len() == 1 {
                state.set_var(var_names[0], &line);
            } else {
                let parts: Vec<&str> = line.split(delim).collect();
                for (vi, var) in var_names.iter().enumerate() {
                    state.set_var(var, parts.get(vi).unwrap_or(&""));
                }
            }
            0
        }
        _ => 1,
    }
}

fn read_line_with_delimiter(
    delim: char,
    raw: bool,
    var_names: &[&str],
    read_array: bool,
    state: &mut ShellState,
) -> i32 {
    let mut stdin = std::io::stdin().lock();
    let mut bytes = Vec::new();
    let mut encoded_delim = [0_u8; 4];
    let delimiter = delim.encode_utf8(&mut encoded_delim).as_bytes();

    let read_status = loop {
        let mut byte = [0_u8; 1];
        match read_interruptibly(&mut stdin, &mut byte) {
            Ok(0) if bytes.is_empty() => break Err(1),
            Ok(0) => break Ok(()),
            Ok(_) => {
                bytes.push(byte[0]);
                if bytes.ends_with(delimiter) {
                    bytes.truncate(bytes.len() - delimiter.len());
                    break Ok(());
                }
            }
            Err(status) => break Err(status),
        }
    };

    match read_status {
        Ok(()) => {
            let line = String::from_utf8_lossy(&bytes);
            let line = line.trim_end_matches('\r');
            let line = if !raw {
                line.replace("\\\n", "")
            } else {
                line.to_string()
            };

            if read_array {
                // Get IFS for splitting
                let ifs = state.get_var("IFS").unwrap_or(" \t\n");
                let parts: Vec<&str> = line
                    .split(|c: char| ifs.contains(c))
                    .filter(|s| !s.is_empty())
                    .collect();
                if let Some(arr_name) = var_names.first() {
                    state.arrays.insert(
                        arr_name.to_string(),
                        parts.into_iter().map(|s| s.to_string()).collect(),
                    );
                }
            } else if var_names.len() == 1 {
                state.set_var(var_names[0], &line);
            } else {
                // Get IFS for splitting
                let ifs = state.get_var("IFS").unwrap_or(" \t\n");
                let parts: Vec<&str> = line
                    .split(|c: char| ifs.contains(c))
                    .filter(|s| !s.is_empty())
                    .collect();
                for (vi, var) in var_names.iter().enumerate() {
                    state.set_var(var, parts.get(vi).unwrap_or(&""));
                }
            }
            0
        }
        Err(status) => status,
    }
}

fn builtin_test(args: &[String]) -> i32 {
    let args: Vec<&str> = args
        .iter()
        .map(|s| s.as_str())
        .filter(|s| *s != "]")
        .collect();

    if args.is_empty() {
        return 1;
    }

    match parse_test_expr(&args, 0).0 {
        TestResult::True => 0,
        TestResult::False => 1,
        TestResult::Error => 2,
    }
}

#[derive(Debug, Clone, Copy)]
enum TestResult {
    True,
    False,
    Error,
}

fn parse_test_expr(args: &[&str], idx: usize) -> (TestResult, usize) {
    let (result, new_idx) = parse_or_expr(args, idx);
    (result, new_idx)
}

fn parse_or_expr(args: &[&str], idx: usize) -> (TestResult, usize) {
    let (mut left, mut new_idx) = parse_and_expr(args, idx);

    while new_idx < args.len() && args[new_idx] == "-o" {
        new_idx += 1;
        let (right, next_idx) = parse_and_expr(args, new_idx);

        left = match (left, right) {
            (TestResult::True, _) => TestResult::True,
            (_, TestResult::True) => TestResult::True,
            (TestResult::False, TestResult::False) => TestResult::False,
            _ => TestResult::Error,
        };
        new_idx = next_idx;
    }

    (left, new_idx)
}

fn parse_and_expr(args: &[&str], idx: usize) -> (TestResult, usize) {
    let (mut left, mut new_idx) = parse_primary(args, idx);

    while new_idx < args.len() && args[new_idx] == "-a" {
        new_idx += 1;
        let (right, next_idx) = parse_primary(args, new_idx);

        left = match (left, right) {
            (TestResult::False, _) => TestResult::False,
            (_, TestResult::False) => TestResult::False,
            (TestResult::True, TestResult::True) => TestResult::True,
            _ => TestResult::Error,
        };
        new_idx = next_idx;
    }

    (left, new_idx)
}

fn parse_primary(args: &[&str], idx: usize) -> (TestResult, usize) {
    if idx >= args.len() {
        return (TestResult::Error, idx);
    }

    // Handle negation
    if args[idx] == "!" {
        let (result, new_idx) = parse_primary(args, idx + 1);
        let negated = match result {
            TestResult::True => TestResult::False,
            TestResult::False => TestResult::True,
            TestResult::Error => TestResult::Error,
        };
        return (negated, new_idx);
    }

    // Handle parentheses
    if args[idx] == "(" {
        let (result, new_idx) = parse_or_expr(args, idx + 1);
        if new_idx < args.len() && args[new_idx] == ")" {
            return (result, new_idx + 1);
        }
        return (TestResult::Error, new_idx);
    }

    // Handle unary operators
    if idx + 1 < args.len() {
        match args[idx] {
            "-n" => {
                return (
                    if !args[idx + 1].is_empty() {
                        TestResult::True
                    } else {
                        TestResult::False
                    },
                    idx + 2,
                )
            }
            "-z" => {
                return (
                    if args[idx + 1].is_empty() {
                        TestResult::True
                    } else {
                        TestResult::False
                    },
                    idx + 2,
                )
            }
            "-f" => {
                return (
                    if Path::new(args[idx + 1]).is_file() {
                        TestResult::True
                    } else {
                        TestResult::False
                    },
                    idx + 2,
                )
            }
            "-d" => {
                return (
                    if Path::new(args[idx + 1]).is_dir() {
                        TestResult::True
                    } else {
                        TestResult::False
                    },
                    idx + 2,
                )
            }
            "-e" => {
                return (
                    if Path::new(args[idx + 1]).exists() {
                        TestResult::True
                    } else {
                        TestResult::False
                    },
                    idx + 2,
                )
            }
            "-L" => {
                return (
                    if is_symlink(args[idx + 1]) {
                        TestResult::True
                    } else {
                        TestResult::False
                    },
                    idx + 2,
                )
            }
            "-p" => {
                return (
                    if is_fifo(args[idx + 1]) {
                        TestResult::True
                    } else {
                        TestResult::False
                    },
                    idx + 2,
                )
            }
            "-S" => {
                return (
                    if is_socket(args[idx + 1]) {
                        TestResult::True
                    } else {
                        TestResult::False
                    },
                    idx + 2,
                )
            }
            "-b" => {
                return (
                    if is_block_device(args[idx + 1]) {
                        TestResult::True
                    } else {
                        TestResult::False
                    },
                    idx + 2,
                )
            }
            "-c" => {
                return (
                    if is_char_device(args[idx + 1]) {
                        TestResult::True
                    } else {
                        TestResult::False
                    },
                    idx + 2,
                )
            }
            "-s" => {
                let result = if let Ok(m) = std::fs::metadata(args[idx + 1]) {
                    if m.len() > 0 {
                        TestResult::True
                    } else {
                        TestResult::False
                    }
                } else {
                    TestResult::False
                };
                return (result, idx + 2);
            }
            "-r" => {
                return (
                    if is_readable(args[idx + 1]) {
                        TestResult::True
                    } else {
                        TestResult::False
                    },
                    idx + 2,
                )
            }
            "-w" => {
                return (
                    if is_writable(args[idx + 1]) {
                        TestResult::True
                    } else {
                        TestResult::False
                    },
                    idx + 2,
                )
            }
            "-x" => {
                return (
                    if is_executable(args[idx + 1]) {
                        TestResult::True
                    } else {
                        TestResult::False
                    },
                    idx + 2,
                )
            }
            _ => {}
        }
    }

    // Handle binary operators
    if idx + 2 < args.len() {
        match args[idx + 1] {
            "=" | "==" => {
                return (
                    if args[idx] == args[idx + 2] {
                        TestResult::True
                    } else {
                        TestResult::False
                    },
                    idx + 3,
                )
            }
            "!=" => {
                return (
                    if args[idx] != args[idx + 2] {
                        TestResult::True
                    } else {
                        TestResult::False
                    },
                    idx + 3,
                )
            }
            "<" => {
                return (
                    if args[idx] < args[idx + 2] {
                        TestResult::True
                    } else {
                        TestResult::False
                    },
                    idx + 3,
                )
            }
            ">" => {
                return (
                    if args[idx] > args[idx + 2] {
                        TestResult::True
                    } else {
                        TestResult::False
                    },
                    idx + 3,
                )
            }
            "-eq" => {
                let result = match (args[idx].parse::<i64>(), args[idx + 2].parse::<i64>()) {
                    (Ok(a), Ok(b)) => {
                        if a == b {
                            TestResult::True
                        } else {
                            TestResult::False
                        }
                    }
                    _ => TestResult::Error,
                };
                return (result, idx + 3);
            }
            "-ne" => {
                let result = match (args[idx].parse::<i64>(), args[idx + 2].parse::<i64>()) {
                    (Ok(a), Ok(b)) => {
                        if a != b {
                            TestResult::True
                        } else {
                            TestResult::False
                        }
                    }
                    _ => TestResult::Error,
                };
                return (result, idx + 3);
            }
            "-lt" => {
                let result = match (args[idx].parse::<i64>(), args[idx + 2].parse::<i64>()) {
                    (Ok(a), Ok(b)) => {
                        if a < b {
                            TestResult::True
                        } else {
                            TestResult::False
                        }
                    }
                    _ => TestResult::Error,
                };
                return (result, idx + 3);
            }
            "-le" => {
                let result = match (args[idx].parse::<i64>(), args[idx + 2].parse::<i64>()) {
                    (Ok(a), Ok(b)) => {
                        if a <= b {
                            TestResult::True
                        } else {
                            TestResult::False
                        }
                    }
                    _ => TestResult::Error,
                };
                return (result, idx + 3);
            }
            "-gt" => {
                let result = match (args[idx].parse::<i64>(), args[idx + 2].parse::<i64>()) {
                    (Ok(a), Ok(b)) => {
                        if a > b {
                            TestResult::True
                        } else {
                            TestResult::False
                        }
                    }
                    _ => TestResult::Error,
                };
                return (result, idx + 3);
            }
            "-ge" => {
                let result = match (args[idx].parse::<i64>(), args[idx + 2].parse::<i64>()) {
                    (Ok(a), Ok(b)) => {
                        if a >= b {
                            TestResult::True
                        } else {
                            TestResult::False
                        }
                    }
                    _ => TestResult::Error,
                };
                return (result, idx + 3);
            }
            _ => {}
        }
    }

    // Single argument - check if non-empty string
    if idx + 1 == args.len() {
        return (
            if !args[idx].is_empty() {
                TestResult::True
            } else {
                TestResult::False
            },
            idx + 1,
        );
    }

    (TestResult::Error, idx)
}

fn is_symlink(path: &str) -> bool {
    std::fs::symlink_metadata(path)
        .map(|m| m.file_type().is_symlink())
        .unwrap_or(false)
}

fn is_fifo(path: &str) -> bool {
    use nix::sys::stat;
    if let Ok(stat) = stat::stat(path) {
        stat.st_mode & 0o170000 == 0o10000
    } else {
        false
    }
}

fn is_socket(path: &str) -> bool {
    use nix::sys::stat;
    if let Ok(stat) = stat::stat(path) {
        stat.st_mode & 0o170000 == 0o140000
    } else {
        false
    }
}

fn is_block_device(path: &str) -> bool {
    use nix::sys::stat;
    if let Ok(stat) = stat::stat(path) {
        stat.st_mode & 0o170000 == 0o60000
    } else {
        false
    }
}

fn is_char_device(path: &str) -> bool {
    use nix::sys::stat;
    if let Ok(stat) = stat::stat(path) {
        stat.st_mode & 0o170000 == 0o20000
    } else {
        false
    }
}

fn is_readable(path: &str) -> bool {
    use nix::unistd;
    unistd::access(std::path::Path::new(path), unistd::AccessFlags::R_OK).is_ok()
}

fn is_writable(path: &str) -> bool {
    use nix::unistd;
    unistd::access(std::path::Path::new(path), unistd::AccessFlags::W_OK).is_ok()
}

fn is_executable(path: &str) -> bool {
    use nix::unistd;
    unistd::access(std::path::Path::new(path), unistd::AccessFlags::X_OK).is_ok()
}

fn cmp_int(a: &str, b: &str, f: fn(i64, i64) -> bool) -> i32 {
    match (a.parse::<i64>(), b.parse::<i64>()) {
        (Ok(a), Ok(b)) => {
            if f(a, b) {
                0
            } else {
                1
            }
        }
        _ => 2,
    }
}

fn builtin_set(args: &[String], state: &mut ShellState) -> i32 {
    if args.is_empty() {
        let mut all: Vec<_> = state.env_vars.iter().collect();
        // Also include variables from all local scopes
        for scope in &state.local_vars_stack {
            all.extend(scope.iter());
        }
        all.sort_by_key(|(k, _)| (*k).clone());
        for (k, v) in all {
            println!("{}={}", k, v);
        }
        return 0;
    }
    let mut i = 0;
    while i < args.len() {
        match args[i].as_str() {
            "-e" => state.shell_opts.errexit = true,
            "+e" => state.shell_opts.errexit = false,
            "-x" => state.shell_opts.xtrace = true,
            "+x" => state.shell_opts.xtrace = false,
            "-o" => {
                i += 1;
                if i < args.len() {
                    match args[i].as_str() {
                        "errexit" => state.shell_opts.errexit = true,
                        "xtrace" => state.shell_opts.xtrace = true,
                        "pipefail" => state.shell_opts.pipefail = true,
                        "globstar" => state.shell_opts.globstar = true,
                        "vi" => state.editing_mode = crate::environment::EditingMode::Vi,
                        "emacs" => state.editing_mode = crate::environment::EditingMode::Emacs,
                        _ => {
                            eprintln!("jsh: set: unknown option: {}", args[i]);
                            return 1;
                        }
                    }
                }
            }
            "+o" => {
                i += 1;
                if i < args.len() {
                    match args[i].as_str() {
                        "errexit" => state.shell_opts.errexit = false,
                        "xtrace" => state.shell_opts.xtrace = false,
                        "pipefail" => state.shell_opts.pipefail = false,
                        "globstar" => state.shell_opts.globstar = false,
                        "vi" => state.editing_mode = crate::environment::EditingMode::Emacs,
                        "emacs" => state.editing_mode = crate::environment::EditingMode::Vi,
                        _ => {
                            eprintln!("jsh: set: unknown option: {}", args[i]);
                            return 1;
                        }
                    }
                }
            }
            "--" => {
                state.positional_params = args[i + 1..].to_vec();
                return 0;
            }
            _ => {
                state.positional_params = args[i..].to_vec();
                return 0;
            }
        }
        i += 1;
    }
    0
}

fn builtin_local(args: &[String], state: &mut ShellState) -> i32 {
    for arg in args {
        if let Some(eq_pos) = arg.find('=') {
            let name = &arg[..eq_pos];
            let value = &arg[eq_pos + 1..];
            if let Some(scope) = state.local_vars_stack.last_mut() {
                scope.insert(name.to_string(), value.to_string());
            }
        } else {
            if let Some(scope) = state.local_vars_stack.last_mut() {
                scope.insert(arg.clone(), String::new());
            }
        }
    }
    0
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

fn builtin_printf(args: &[String]) -> i32 {
    use std::io::Write;
    if args.is_empty() {
        return 0;
    }
    let fmt = &args[0];
    let params = &args[1..];
    let mut out = String::new();
    let mut pi = 0;
    // Reuse the format string over remaining arguments, like bash printf.
    loop {
        let start_pi = pi;
        let mut consumed_conversion = false;
        let mut chars = fmt.chars().peekable();
        while let Some(c) = chars.next() {
            if c == '\\' {
                match chars.next() {
                    Some('n') => out.push('\n'),
                    Some('t') => out.push('\t'),
                    Some('r') => out.push('\r'),
                    Some('\\') => out.push('\\'),
                    Some('0') => out.push('\0'),
                    Some('a') => out.push('\x07'),
                    Some('b') => out.push('\x08'),
                    Some(c2) => {
                        out.push('\\');
                        out.push(c2);
                    }
                    None => out.push('\\'),
                }
            } else if c == '%' {
                let arg = params.get(pi).map(|s| s.as_str()).unwrap_or("");
                match chars.next() {
                    Some('s') => out.push_str(arg),
                    Some('d') | Some('i') => {
                        out.push_str(&arg.parse::<i64>().unwrap_or(0).to_string())
                    }
                    Some('f') => out.push_str(&arg.parse::<f64>().unwrap_or(0.0).to_string()),
                    Some('x') => out.push_str(&format!("{:x}", arg.parse::<i64>().unwrap_or(0))),
                    Some('X') => out.push_str(&format!("{:X}", arg.parse::<i64>().unwrap_or(0))),
                    Some('o') => out.push_str(&format!("{:o}", arg.parse::<i64>().unwrap_or(0))),
                    Some('c') => out.push(arg.chars().next().unwrap_or('\0')),
                    Some('%') => {
                        out.push('%');
                        continue;
                    }
                    Some(c2) => {
                        out.push('%');
                        out.push(c2);
                    }
                    None => out.push('%'),
                }
                pi += 1;
                consumed_conversion = true;
            } else {
                out.push(c);
            }
        }
        // A format with no arg-consuming conversion prints exactly once; otherwise
        // repeat until all arguments are consumed.
        if !consumed_conversion || pi >= params.len() || pi == start_pi {
            break;
        }
    }
    print!("{}", out);
    std::io::stdout().flush().ok();
    0
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

fn builtin_exec(args: &[String], _state: &mut ShellState) -> i32 {
    use nix::unistd::close;
    use std::fs::{File, OpenOptions};
    use std::os::unix::io::{IntoRawFd, RawFd};

    fn dup2_raw(oldfd: RawFd, newfd: RawFd) -> Result<(), String> {
        unsafe {
            match nix::libc::dup2(oldfd, newfd) {
                -1 => Err(format!("dup2 failed")),
                _ => Ok(()),
            }
        }
    }

    if args.is_empty() {
        return 0;
    }

    // Simple implementation of exec FD redirection
    // Format: exec FD<file, exec FD>file, exec FD>&FD2, etc.

    for arg in args {
        // Parse FD redirection: "3<file", "1>file", "2>&1", "{fd}>&-", etc.
        let (fd_str, redirect_type, target) = if let Some(pos) = arg.find('<') {
            let fd = &arg[..pos];
            let target = &arg[pos + 1..];
            (fd, "<", target)
        } else if let Some(pos) = arg.find('>') {
            let fd = &arg[..pos];
            if pos + 1 < arg.len() && arg.chars().nth(pos + 1) == Some('>') {
                // >> redirect
                let target = &arg[pos + 2..];
                (fd, ">>", target)
            } else if pos + 1 < arg.len() && arg.chars().nth(pos + 1) == Some('&') {
                // >& redirect
                let target = &arg[pos + 2..];
                (fd, ">&", target)
            } else {
                // > redirect
                let target = &arg[pos + 1..];
                (fd, ">", target)
            }
        } else {
            continue;
        };

        // Parse the FD number (handle {fd} format)
        let fd_clean = fd_str.trim_matches(|c| c == '{' || c == '}');
        let fd: i32 = match fd_clean.parse() {
            Ok(n) => n,
            Err(_) => {
                eprintln!("jsh: exec: invalid file descriptor: {}", fd_str);
                return 1;
            }
        };

        // Execute the redirection
        match redirect_type {
            "<" => {
                // Input redirection: open for reading
                match File::open(target) {
                    Ok(file) => {
                        let src_fd = file.into_raw_fd();
                        if let Err(_) = dup2_raw(src_fd, fd) {
                            eprintln!("jsh: exec: dup2 failed");
                            return 1;
                        }
                        if src_fd != fd {
                            close(src_fd).ok();
                        }
                    }
                    Err(_) => {
                        eprintln!("jsh: exec: cannot open {} for reading", target);
                        return 1;
                    }
                }
            }
            ">" => {
                // Output redirection: open for writing
                match File::create(target) {
                    Ok(file) => {
                        let src_fd = file.into_raw_fd();
                        if let Err(_) = dup2_raw(src_fd, fd) {
                            eprintln!("jsh: exec: dup2 failed");
                            return 1;
                        }
                        if src_fd != fd {
                            close(src_fd).ok();
                        }
                    }
                    Err(_) => {
                        eprintln!("jsh: exec: cannot open {} for writing", target);
                        return 1;
                    }
                }
            }
            ">>" => {
                // Append redirection
                match OpenOptions::new().create(true).append(true).open(target) {
                    Ok(file) => {
                        let src_fd = file.into_raw_fd();
                        if let Err(_) = dup2_raw(src_fd, fd) {
                            eprintln!("jsh: exec: dup2 failed");
                            return 1;
                        }
                        if src_fd != fd {
                            close(src_fd).ok();
                        }
                    }
                    Err(_) => {
                        eprintln!("jsh: exec: cannot open {} for appending", target);
                        return 1;
                    }
                }
            }
            ">&" => {
                if target == "-" {
                    // Close FD
                    close(fd).ok();
                } else {
                    // Duplicate FD
                    match target.parse::<i32>() {
                        Ok(target_fd) => {
                            if let Err(_) = dup2_raw(target_fd, fd) {
                                eprintln!("jsh: exec: dup2 failed");
                                return 1;
                            }
                        }
                        Err(_) => {
                            eprintln!("jsh: exec: invalid target FD: {}", target);
                            return 1;
                        }
                    }
                }
            }
            _ => {}
        }
    }

    0
}

fn builtin_pushd(args: &[String], state: &mut ShellState) -> i32 {
    let mut print_stack = true;
    let mut args_start = 0;

    // Parse options
    for (i, arg) in args.iter().enumerate() {
        if arg == "-n" {
            print_stack = false;
            args_start = i + 1;
        } else {
            break;
        }
    }

    let remaining_args = &args[args_start..];

    let cwd = env::current_dir().ok();
    let target = if remaining_args.is_empty() {
        // pushd with no args swaps top two directories
        if state.dir_stack.is_empty() {
            eprintln!("jsh: pushd: no other directory");
            return 1;
        }
        match state.dir_stack.pop() {
            Some(d) => d.to_string_lossy().to_string(),
            None => {
                eprintln!("jsh: pushd: no other directory");
                return 1;
            }
        }
    } else if remaining_args[0].starts_with('+') || remaining_args[0].starts_with('-') {
        // Handle stack navigation: pushd +N or pushd -N
        if let Ok(idx) = remaining_args[0][1..].parse::<usize>() {
            if remaining_args[0].starts_with('+') {
                if idx < state.dir_stack.len() {
                    state.dir_stack[idx].to_string_lossy().to_string()
                } else {
                    eprintln!("jsh: pushd: invalid stack index: +{}", idx);
                    return 1;
                }
            } else {
                if idx > 0 && idx <= state.dir_stack.len() {
                    state.dir_stack[state.dir_stack.len() - idx]
                        .to_string_lossy()
                        .to_string()
                } else {
                    eprintln!("jsh: pushd: invalid stack index: -{}", idx);
                    return 1;
                }
            }
        } else {
            remaining_args[0].clone()
        }
    } else {
        remaining_args[0].clone()
    };

    if let Some(cwd) = cwd.as_ref() {
        state.dir_stack.push(cwd.to_path_buf());
    }

    match env::set_current_dir(&target) {
        Ok(()) => {
            if let Ok(new_dir) = env::current_dir() {
                if let Some(old_dir) = cwd.as_ref() {
                    update_directory_vars(Some(old_dir.as_path()), &new_dir, state);
                } else {
                    update_directory_vars(None, &new_dir, state);
                }
            }
            if print_stack {
                builtin_dirs(state);
            }
            0
        }
        Err(e) => {
            eprintln!("jsh: pushd: {}: {}", target, e);
            1
        }
    }
}

fn builtin_popd(state: &mut ShellState) -> i32 {
    if state.dir_stack.is_empty() {
        eprintln!("jsh: popd: directory stack empty");
        return 1;
    }

    match state.dir_stack.pop() {
        Some(dir) => {
            let old_dir = env::current_dir().ok();
            match env::set_current_dir(&dir) {
                Ok(()) => {
                    if let Ok(new_dir) = env::current_dir() {
                        update_directory_vars(old_dir.as_deref(), &new_dir, state);
                    }
                    builtin_dirs(state);
                    0
                }
                Err(e) => {
                    eprintln!("jsh: popd: {}", e);
                    1
                }
            }
        }
        None => {
            eprintln!("jsh: popd: directory stack empty");
            1
        }
    }
}

fn builtin_dirs(state: &ShellState) -> i32 {
    let cwd = env::current_dir()
        .map(|p| p.to_string_lossy().to_string())
        .unwrap_or_default();
    print!("{}", cwd);
    for d in state.dir_stack.iter().rev() {
        print!(" {}", d.display());
    }
    println!();
    0
}

fn builtin_trap(args: &[String], state: &mut ShellState) -> i32 {
    if args.is_empty() {
        for (sig, cmd) in &state.traps {
            println!("trap -- '{}' {}", cmd, sig);
        }
        return 0;
    }

    if args.len() == 1 && args[0] == "-l" {
        println!("EXIT HUP INT QUIT ABRT ALRM TERM USR1 USR2");
        return 0;
    }

    if args.len() == 1 && args[0] == "-p" {
        for (sig, cmd) in &state.traps {
            println!("trap -- '{}' {}", cmd, sig);
        }
        return 0;
    }

    if args.len() >= 2 {
        let action = &args[0];
        for sig in &args[1..] {
            // Validate signal name
            let sig_lower = sig.to_uppercase();
            let valid_signals = vec![
                "EXIT", "HUP", "INT", "QUIT", "ABRT", "ALRM", "TERM", "USR1", "USR2", "PIPE",
                "CHLD", "TSTP", "TTIN", "TTOU", "CONT", "STOP", "KILL", "ILL", "FPE", "SEGV",
                "BUS", "SYS", "TRAP", "CLD", "PWR", "POLL", "PROF", "VTALRM", "XCPU", "XFSZ",
                "IOT", "EMT", "STKFLT", "IO", "ERR", "RETURN", "DEBUG",
            ];

            let is_valid =
                valid_signals.iter().any(|&s| s == sig_lower) || sig_lower.parse::<i32>().is_ok();

            if !is_valid {
                eprintln!("jsh: trap: {} is not a valid signal name", sig);
                return 1;
            }

            if action == "-" || action.is_empty() {
                state.traps.remove(&sig_lower);
            } else {
                state.traps.insert(sig_lower, action.clone());
            }
        }
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
    eval_cond_expr(&args, &mut 0, state)
}

fn eval_cond_expr(args: &[&str], pos: &mut usize, state: &mut ShellState) -> i32 {
    eval_cond_or(args, pos, state)
}

fn eval_cond_or(args: &[&str], pos: &mut usize, state: &mut ShellState) -> i32 {
    let mut left = eval_cond_and(args, pos, state);
    while *pos < args.len() && args[*pos] == "||" {
        *pos += 1;
        let right = eval_cond_and(args, pos, state);
        left = if left == 0 || right == 0 { 0 } else { 1 };
    }
    left
}

fn eval_cond_and(args: &[&str], pos: &mut usize, state: &mut ShellState) -> i32 {
    let mut left = eval_cond_primary(args, pos, state);
    while *pos < args.len() && args[*pos] == "&&" {
        *pos += 1;
        let right = eval_cond_primary(args, pos, state);
        left = if left == 0 && right == 0 { 0 } else { 1 };
    }
    left
}

fn eval_cond_primary(args: &[&str], pos: &mut usize, state: &mut ShellState) -> i32 {
    if *pos >= args.len() {
        return 1;
    }

    if args[*pos] == "!" {
        *pos += 1;
        return eval_cond_primary(args, pos, state) ^ 1;
    }

    if args[*pos] == "(" {
        *pos += 1;
        let r = eval_cond_expr(args, pos, state);
        if *pos < args.len() && args[*pos] == ")" {
            *pos += 1;
        }
        return r;
    }

    // Unary operators
    if args[*pos].starts_with('-') && args[*pos].len() == 2 && *pos + 1 < args.len() {
        let op = args[*pos];
        let operand = args[*pos + 1];
        let result = match op {
            "-n" => {
                *pos += 2;
                if !operand.is_empty() {
                    0
                } else {
                    1
                }
            }
            "-z" => {
                *pos += 2;
                if operand.is_empty() {
                    0
                } else {
                    1
                }
            }
            "-f" => {
                *pos += 2;
                if Path::new(operand).is_file() {
                    0
                } else {
                    1
                }
            }
            "-d" => {
                *pos += 2;
                if Path::new(operand).is_dir() {
                    0
                } else {
                    1
                }
            }
            "-e" => {
                *pos += 2;
                if Path::new(operand).exists() {
                    0
                } else {
                    1
                }
            }
            "-s" => {
                *pos += 2;
                std::fs::metadata(operand)
                    .map(|m| if m.len() > 0 { 0 } else { 1 })
                    .unwrap_or(1)
            }
            "-r" | "-w" | "-x" => {
                *pos += 2;
                if Path::new(operand).exists() {
                    0
                } else {
                    1
                }
            }
            _ => {
                if *pos + 2 < args.len() {
                    return eval_cond_binary(args, pos, state);
                }
                *pos += 2;
                1
            }
        };
        return result;
    }

    // Binary expression or standalone string test
    if *pos + 1 < args.len() && is_cond_binary_op(args[*pos + 1]) {
        return eval_cond_binary(args, pos, state);
    }

    let s = args[*pos];
    *pos += 1;
    if s.is_empty() {
        1
    } else {
        0
    }
}

fn is_cond_binary_op(op: &str) -> bool {
    matches!(
        op,
        "==" | "=" | "!=" | "<" | ">" | "-eq" | "-ne" | "-lt" | "-le" | "-gt" | "-ge" | "=~"
    )
}

fn eval_cond_binary(args: &[&str], pos: &mut usize, state: &mut ShellState) -> i32 {
    if *pos + 2 > args.len() {
        return 1;
    }
    let left = args[*pos];
    let op = args[*pos + 1];
    let right = args[*pos + 2];
    *pos += 3;
    match op {
        "==" | "=" => {
            if right.contains('*') || right.contains('?') {
                if glob_match(right, left) {
                    0
                } else {
                    1
                }
            } else {
                if left == right {
                    0
                } else {
                    1
                }
            }
        }
        "!=" => {
            if right.contains('*') || right.contains('?') {
                if glob_match(right, left) {
                    1
                } else {
                    0
                }
            } else {
                if left != right {
                    0
                } else {
                    1
                }
            }
        }
        "<" => {
            if left < right {
                0
            } else {
                1
            }
        }
        ">" => {
            if left > right {
                0
            } else {
                1
            }
        }
        "-eq" => cmp_int(left, right, |a, b| a == b),
        "-ne" => cmp_int(left, right, |a, b| a != b),
        "-lt" => cmp_int(left, right, |a, b| a < b),
        "-le" => cmp_int(left, right, |a, b| a <= b),
        "-gt" => cmp_int(left, right, |a, b| a > b),
        "-ge" => cmp_int(left, right, |a, b| a >= b),
        "=~" => {
            // Real regex matching with BASH_REMATCH
            match regex::Regex::new(right) {
                Ok(re) => {
                    if let Some(captures) = re.captures(left) {
                        // Store BASH_REMATCH array
                        let mut rematch = Vec::new();
                        for i in 0..captures.len() {
                            rematch.push(
                                captures
                                    .get(i)
                                    .map(|m| m.as_str().to_string())
                                    .unwrap_or_default(),
                            );
                        }
                        state.arrays.insert("BASH_REMATCH".to_string(), rematch);
                        0
                    } else {
                        state.arrays.insert("BASH_REMATCH".to_string(), Vec::new());
                        1
                    }
                }
                Err(_) => 2,
            }
        }
        _ => 1,
    }
}

fn glob_match(pattern: &str, text: &str) -> bool {
    crate::glob_match::glob_match(pattern, text)
}

// ============================================================
// declare (Phase 1)
// ============================================================

fn builtin_declare(args: &[String], state: &mut ShellState) -> i32 {
    let mut indexed = false;
    let mut associative = false;
    let mut print = false;
    let mut names: Vec<&str> = Vec::new();
    let mut i = 0;

    while i < args.len() {
        match args[i].as_str() {
            "-a" => indexed = true,
            "-A" => associative = true,
            "-p" => print = true,
            s => names.push(s),
        }
        i += 1;
    }

    if print {
        for name in &names {
            if let Some(arr) = state.arrays.get(*name) {
                println!(
                    "declare -a {}=({})",
                    name,
                    arr.iter()
                        .enumerate()
                        .map(|(i, s)| format!("[{}]=\"{}\"", i, s))
                        .collect::<Vec<_>>()
                        .join(" ")
                );
            } else if let Some(map) = state.assoc_arrays.get(*name) {
                let pairs: Vec<String> = map
                    .iter()
                    .map(|(k, v)| format!("[{}]=\"{}\"", k, v))
                    .collect();
                println!("declare -A {}=({})", name, pairs.join(" "));
            } else if let Some(val) = state.get_var(name) {
                println!("declare -- {}=\"{}\"", name, val);
            }
        }
        return 0;
    }

    for name in &names {
        // Handle name=value or name=()
        let (var_name, value) = if let Some(eq) = name.find('=') {
            (&name[..eq], Some(&name[eq + 1..]))
        } else {
            (*name, None)
        };

        if associative {
            if !state.assoc_arrays.contains_key(var_name) {
                state
                    .assoc_arrays
                    .insert(var_name.to_string(), std::collections::HashMap::new());
            }

            // Parse initialization value like: ([key1]=val1 [key2]=val2)
            if let Some(val) = value {
                if val.starts_with('(') && val.ends_with(')') {
                    let inner = &val[1..val.len() - 1].trim();
                    parse_assoc_array_init(var_name, inner, state);
                } else if !val.is_empty() && !val.starts_with('(') {
                    // Handle single value assignment (rare for assoc arrays)
                    state
                        .assoc_arrays
                        .get_mut(var_name)
                        .unwrap()
                        .insert("0".to_string(), val.to_string());
                }
            }
        } else if indexed {
            if !state.arrays.contains_key(var_name) {
                state.arrays.insert(var_name.to_string(), Vec::new());
            }

            // Parse initialization value like: (val1 val2 val3)
            if let Some(val) = value {
                if val.starts_with('(') && val.ends_with(')') {
                    let inner = &val[1..val.len() - 1];
                    let elements: Vec<&str> = inner.split_whitespace().collect();
                    *state.arrays.get_mut(var_name).unwrap() =
                        elements.iter().map(|s| s.to_string()).collect();
                } else if !val.is_empty() && !val.starts_with('(') {
                    // Single value
                    state
                        .arrays
                        .get_mut(var_name)
                        .unwrap()
                        .push(val.to_string());
                }
            }
        } else {
            // Regular variable
            if let Some(val) = value {
                state.set_var(var_name, val);
            }
        }
    }
    0
}

fn parse_assoc_array_init(var_name: &str, input: &str, state: &mut ShellState) {
    // Parse input like: [key1]=val1 [key2]=val2
    let mut current = input;
    while !current.is_empty() {
        current = current.trim_start();
        if !current.starts_with('[') {
            break;
        }

        // Find closing bracket
        if let Some(bracket_end) = current.find(']') {
            let key = &current[1..bracket_end];
            let rest = &current[bracket_end + 1..];

            // Skip = sign
            if rest.starts_with('=') {
                let value_part = &rest[1..].trim_start();

                // Extract value (quoted or unquoted)
                let (value, next_pos) = if value_part.starts_with('"') {
                    // Quoted value
                    let mut escaped = false;
                    let mut end_pos = 1;
                    for (i, ch) in value_part[1..].char_indices() {
                        if escaped {
                            escaped = false;
                        } else if ch == '\\' {
                            escaped = true;
                        } else if ch == '"' {
                            end_pos = i + 2;
                            break;
                        }
                    }
                    let val = &value_part[1..end_pos - 1];
                    (val.to_string(), end_pos)
                } else if value_part.starts_with('\'') {
                    // Single-quoted value
                    if let Some(end) = value_part[1..].find('\'') {
                        let val = &value_part[1..end + 1];
                        (val.to_string(), end + 2)
                    } else {
                        break;
                    }
                } else {
                    // Unquoted value (until space or next bracket)
                    let end_pos = value_part
                        .find(|c: char| c == ' ' || c == '[')
                        .unwrap_or(value_part.len());
                    (value_part[..end_pos].to_string(), end_pos)
                };

                // Store in associative array
                state
                    .assoc_arrays
                    .get_mut(var_name)
                    .unwrap()
                    .insert(key.to_string(), value);

                current = &value_part[next_pos..];
            } else {
                break;
            }
        } else {
            break;
        }
    }
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
            z_db.remove(&args[1]);
            return 0;
        }

        if args.len() == 1 && args[0] == "-c" {
            if let Ok(cwd) = env::current_dir() {
                z_db.remove(&cwd.to_string_lossy());
            }
            return 0;
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
    let mut remove = false;
    let mut command_name = String::new();
    let mut i = 0;

    while i < args.len() {
        match args[i].as_str() {
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
            _ => command_name = args[i].clone(),
        }
        i += 1;
    }

    if command_name.is_empty() {
        eprintln!("jsh: complete: no command specified");
        return 1;
    }

    if remove {
        state.completion_specs.remove(&command_name);
        return 0;
    }

    state.completion_specs.insert(
        command_name.clone(),
        crate::environment::CompletionSpec {
            command: command_name,
            word_list,
            function,
            directory,
            file,
            glob_pattern,
            filter_pattern,
            prefix,
            suffix,
        },
    );
    0
}

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
            "-c" => {
                action = Some("command");
            }
            "-b" => {
                action = Some("builtin");
            }
            "-a" => {
                action = Some("alias");
            }
            "-d" => {
                action = Some("directory");
            }
            "-f" => {
                action = Some("file");
            }
            "-v" => {
                action = Some("variable");
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

    if let Some(act) = action {
        match act {
            "command" => {
                let cache = state.path_cache().clone();
                results.extend(cache.into_iter().filter(|c| c.starts_with(prefix)));
                results.extend(
                    BUILTIN_NAMES
                        .iter()
                        .filter(|b| b.starts_with(prefix))
                        .map(|s| s.to_string()),
                );
            }
            "builtin" => {
                results.extend(
                    BUILTIN_NAMES
                        .iter()
                        .filter(|b| b.starts_with(prefix))
                        .map(|s| s.to_string()),
                );
            }
            "alias" => {
                results.extend(
                    state
                        .aliases
                        .keys()
                        .filter(|a| a.starts_with(prefix))
                        .cloned(),
                );
            }
            "function" => {
                results.extend(
                    state
                        .functions
                        .keys()
                        .filter(|f| f.starts_with(prefix))
                        .cloned(),
                );
            }
            "directory" => {
                if let Ok(entries) = std::fs::read_dir(".") {
                    for entry in entries.flatten() {
                        if let Ok(ft) = entry.file_type() {
                            if ft.is_dir() {
                                if let Some(name) = entry.file_name().to_str() {
                                    if name.starts_with(prefix) {
                                        results.push(name.to_string());
                                    }
                                }
                            }
                        }
                    }
                }
            }
            "file" => {
                if let Ok(entries) = std::fs::read_dir(".") {
                    for entry in entries.flatten() {
                        if let Some(name) = entry.file_name().to_str() {
                            if name.starts_with(prefix) {
                                results.push(name.to_string());
                            }
                        }
                    }
                }
            }
            "variable" => {
                results.extend(
                    state
                        .env_vars
                        .keys()
                        .filter(|v| v.starts_with(prefix))
                        .cloned(),
                );
            }
            _ => {}
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

    for word in &word_list {
        if word.starts_with(prefix) {
            results.push(word.clone());
        }
    }

    results.sort();
    results.dedup();
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

    let id: Option<usize> = args[0].trim_start_matches('%').parse().ok();
    match id {
        Some(id) => {
            state.jobs.jobs.retain(|j| j.id != id);
            0
        }
        None => {
            eprintln!("jsh: disown: {}: no such job", args[0]);
            1
        }
    }
}

fn builtin_wait(args: &[String], state: &mut ShellState) -> i32 {
    use nix::sys::wait::{waitpid, WaitPidFlag, WaitStatus};
    use nix::unistd::Pid;

    if args.is_empty() {
        loop {
            match waitpid(Pid::from_raw(-1), Some(WaitPidFlag::WNOHANG)) {
                Ok(WaitStatus::Exited(pid, _)) | Ok(WaitStatus::Signaled(pid, _, _)) => {
                    state.jobs.jobs.retain(|j| j.pid != pid);
                }
                _ => break,
            }
        }
        return state.last_exit_code;
    }

    let mut last_status = 0;
    for arg in args {
        let pid_raw = if arg.starts_with('%') {
            let id: Option<usize> = arg.trim_start_matches('%').parse().ok();
            match id.and_then(|id| state.jobs.get_by_id(id)) {
                Some(job) => job.pid.as_raw(),
                None => {
                    eprintln!("jsh: wait: {}: no such job", arg);
                    last_status = 127;
                    continue;
                }
            }
        } else {
            match arg.parse::<i32>() {
                Ok(p) => p,
                Err(_) => {
                    eprintln!("jsh: wait: {}: not a pid or valid job spec", arg);
                    last_status = 127;
                    continue;
                }
            }
        };

        match waitpid(Pid::from_raw(pid_raw), None) {
            Ok(WaitStatus::Exited(pid, code)) => {
                state.jobs.jobs.retain(|j| j.pid != pid);
                last_status = code;
            }
            Ok(WaitStatus::Signaled(pid, sig, _)) => {
                state.jobs.jobs.retain(|j| j.pid != pid);
                last_status = 128 + sig as i32;
            }
            _ => {
                last_status = 127;
            }
        }
    }
    last_status
}

// ============================================================
// shopt (shell options)
// ============================================================

fn builtin_shopt(args: &[String], state: &mut ShellState) -> i32 {
    // shopt [-psuE] [optname ...]
    // -p: print all options (default when no args)
    // -s: set option
    // -u: unset option

    if args.is_empty() {
        // Print all options
        print_shopt_options(&state.shell_opts);
        return 0;
    }

    let mut setting = None;
    let mut print_all = false;
    let mut opts = Vec::new();
    let mut i = 0;

    while i < args.len() {
        match args[i].as_str() {
            "-s" => setting = Some(true),
            "-u" => setting = Some(false),
            "-p" => print_all = true,
            s if s.starts_with('-') => {
                // Unknown option, treat as option name (for compat)
                opts.push(s[1..].to_string());
            }
            _ => opts.push(args[i].clone()),
        }
        i += 1;
    }

    if print_all {
        print_shopt_options(&state.shell_opts);
        return 0;
    }

    if opts.is_empty() {
        // No options specified, print all
        print_shopt_options(&state.shell_opts);
        return 0;
    }

    let mut exit_code = 0;
    for opt in opts {
        match setting {
            Some(true) => {
                // Set option
                match opt.as_str() {
                    "dotglob" => state.shell_opts.dotglob = true,
                    "nullglob" => state.shell_opts.nullglob = true,
                    "failglob" => state.shell_opts.failglob = true,
                    "extglob" => state.shell_opts.extglob = true,
                    "nocaseglob" => state.shell_opts.nocaseglob = true,
                    "noglob" => state.shell_opts.noglob = true,
                    "lastpipe" => state.shell_opts.lastpipe = true,
                    "autocd" => state.shell_opts.autocd = true,
                    "cdspell" => state.shell_opts.cdspell = true,
                    "checkwinsize" => state.shell_opts.checkwinsize = true,
                    "inherit_errexit" => state.shell_opts.inherit_errexit = true,
                    _ => {
                        eprintln!("jsh: shopt: {}: invalid option name", opt);
                        exit_code = 1;
                    }
                }
            }
            Some(false) => {
                // Unset option
                match opt.as_str() {
                    "dotglob" => state.shell_opts.dotglob = false,
                    "nullglob" => state.shell_opts.nullglob = false,
                    "failglob" => state.shell_opts.failglob = false,
                    "extglob" => state.shell_opts.extglob = false,
                    "nocaseglob" => state.shell_opts.nocaseglob = false,
                    "noglob" => state.shell_opts.noglob = false,
                    "lastpipe" => state.shell_opts.lastpipe = false,
                    "autocd" => state.shell_opts.autocd = false,
                    "cdspell" => state.shell_opts.cdspell = false,
                    "checkwinsize" => state.shell_opts.checkwinsize = false,
                    "inherit_errexit" => state.shell_opts.inherit_errexit = false,
                    _ => {
                        eprintln!("jsh: shopt: {}: invalid option name", opt);
                        exit_code = 1;
                    }
                }
            }
            None => {
                // No -s or -u specified, just report status
                let value = match opt.as_str() {
                    "dotglob" => Some(state.shell_opts.dotglob),
                    "nullglob" => Some(state.shell_opts.nullglob),
                    "failglob" => Some(state.shell_opts.failglob),
                    "extglob" => Some(state.shell_opts.extglob),
                    "nocaseglob" => Some(state.shell_opts.nocaseglob),
                    "noglob" => Some(state.shell_opts.noglob),
                    "lastpipe" => Some(state.shell_opts.lastpipe),
                    "autocd" => Some(state.shell_opts.autocd),
                    "cdspell" => Some(state.shell_opts.cdspell),
                    "checkwinsize" => Some(state.shell_opts.checkwinsize),
                    "inherit_errexit" => Some(state.shell_opts.inherit_errexit),
                    _ => None,
                };

                match value {
                    Some(true) => println!("{}\ton", opt),
                    Some(false) => println!("{}\toff", opt),
                    None => {
                        eprintln!("jsh: shopt: {}: invalid option name", opt);
                        exit_code = 1;
                    }
                }
            }
        }
    }

    exit_code
}

fn print_shopt_options(opts: &crate::environment::ShellOpts) {
    // Print all options and their status (like bash)
    let options = vec![
        ("autocd", opts.autocd),
        ("cdspell", opts.cdspell),
        ("checkwinsize", opts.checkwinsize),
        ("dotglob", opts.dotglob),
        ("extglob", opts.extglob),
        ("failglob", opts.failglob),
        ("inherit_errexit", opts.inherit_errexit),
        ("lastpipe", opts.lastpipe),
        ("nocaseglob", opts.nocaseglob),
        ("noglob", opts.noglob),
        ("nullglob", opts.nullglob),
    ];

    for (name, value) in options {
        if value {
            println!("{}\ton", name);
        } else {
            println!("{}\toff", name);
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
            let path = args.get(2).map(|p| p.clone()).unwrap_or_else(|| {
                std::env::current_dir()
                    .map(|p| p.to_string_lossy().to_string())
                    .unwrap_or_else(|_| ".".to_string())
            });
            if let Ok(mut db) = crate::bookmarks::get_bookmark_db().lock() {
                db.add(&name, &path);
                println!("Bookmark '{}' -> {}", name, path);
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
                if db.remove(name) {
                    println!("Removed bookmark '{}'", name);
                    0
                } else {
                    eprintln!("jsh: bookmark '{}' not found", name);
                    1
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

const HELP_ENTRIES: &[(&str, &str)] = &[
    (
        "cd",
        "cd [-] [dir] — Change working directory. cd - returns to previous.",
    ),
    (
        "exit",
        "exit [N] — Exit the shell with status N (default: last command's status).",
    ),
    (
        "export",
        "export [-n] name[=value] — Set environment variables. -n unexports.",
    ),
    ("unset", "unset name... — Remove variables or functions."),
    (
        "echo",
        "echo [-neE] [args...] — Print arguments. -n no newline, -e escapes.",
    ),
    (
        "printf",
        "printf format [args...] — Formatted output (C-style).",
    ),
    ("pwd", "pwd — Print current working directory."),
    (
        "alias",
        "alias [name[=value]...] — Define or display aliases.",
    ),
    (
        "unalias",
        "unalias [-a] name... — Remove aliases. -a removes all.",
    ),
    (
        "type",
        "type name... — Show how each name would be interpreted as a command.",
    ),
    (
        "source",
        "source file [args] — Execute commands from file in current shell.",
    ),
    (
        "eval",
        "eval [args...] — Concatenate args and execute as a command.",
    ),
    (
        "read",
        "read [-p prompt] [-t timeout] [-r] var... — Read line from stdin.",
    ),
    (
        "test",
        "test expr / [ expr ] — Evaluate conditional expression.",
    ),
    (
        "set",
        "set [-/+euxo option] — Set/unset shell options. set -e enables errexit.",
    ),
    (
        "local",
        "local name[=value]... — Declare local variables in a function.",
    ),
    (
        "shift",
        "shift [N] — Shift positional parameters left by N (default 1).",
    ),
    ("jobs", "jobs — List active jobs."),
    ("fg", "fg [%N] — Bring job N to foreground."),
    ("bg", "bg [%N] — Resume job N in background."),
    (
        "wait",
        "wait [pid|%jobspec...] — Wait for processes to complete.",
    ),
    (
        "trap",
        "trap [action] signal... — Set signal handlers. trap '' SIG ignores.",
    ),
    (
        "return",
        "return [N] — Return from a function with exit status N.",
    ),
    ("break", "break [N] — Exit from N enclosing loops."),
    (
        "continue",
        "continue [N] — Resume next iteration of Nth enclosing loop.",
    ),
    (
        "declare",
        "declare [-aAirx] name[=value] — Declare variables with attributes.",
    ),
    ("history", "history — Display command history."),
    (
        "context",
        "context <list|show|last-failed> [options] — Query execution context.",
    ),
    (
        "pushd",
        "pushd [dir] — Push directory onto stack and cd to it.",
    ),
    ("popd", "popd — Pop directory from stack and cd to it."),
    ("dirs", "dirs — Display directory stack."),
    (
        "complete",
        "complete [-W words] [-F func] cmd — Register completions.",
    ),
    (
        "compgen",
        "compgen [-abcdfv] [-A action] [-W words] [-G glob] [prefix]",
    ),
    (
        "disown",
        "disown [-a] [%N] — Remove job from table (won't receive SIGHUP).",
    ),
    (
        "shopt",
        "shopt [-su] opt... — Set/unset shell options (globstar, extglob, etc).",
    ),
    (
        "exec",
        "exec cmd [args] — Replace shell with command (no fork).",
    ),
    (
        "hash",
        "hash [-r] — Refresh command lookup cache (currently a no-op).",
    ),
    (
        "z",
        "z [query] — Jump to frecency-ranked directory matching query.",
    ),
    (
        "bookmark",
        "bookmark <add|go|ls|rm> [name] — Manage directory bookmarks.",
    ),
    (
        "hook",
        "hook <add|remove|list> <precmd|preexec|chpwd> [cmd]",
    ),
    (
        "from-json",
        "from-json — Parse JSON from stdin into structured pipeline.",
    ),
    ("to-json", "to-json — Output structured data as JSON."),
    (
        "to-table",
        "to-table — Output structured data as aligned table.",
    ),
    ("where", "where field op value — Filter structured records."),
    (
        "sort-by",
        "sort-by field — Sort structured records by field.",
    ),
    (
        "select",
        "select field... — Project fields from structured records.",
    ),
];

fn builtin_help(args: &[String], state: &ShellState) -> i32 {
    // Phase 14b: prefer signature-driven help when available.
    if args.is_empty() {
        println!("jsh — a Bash-inspired shell with structured data pipelines\n");
        println!("Core builtins:");
        for (name, desc) in HELP_ENTRIES {
            println!("  {:12} {}", name, desc.split(" — ").nth(1).unwrap_or(desc));
        }
        // Also list signed value-aware commands (Phase 14b).
        let mut signed: Vec<&'static str> = crate::signature::SIGNATURES.keys().copied().collect();
        signed.sort_unstable();
        if !signed.is_empty() {
            println!("\nValue-aware commands (with signatures):");
            // Print 6 per line for compactness.
            for chunk in signed.chunks(6) {
                println!("  {}", chunk.join("  "));
            }
        }
        if !state.user_signatures.is_empty() {
            let mut user: Vec<&str> = state.user_signatures.keys().map(|s| s.as_str()).collect();
            user.sort_unstable();
            println!("\nUser-defined functions:");
            for chunk in user.chunks(6) {
                println!("  {}", chunk.join("  "));
            }
        }
        println!("\nType 'help <command>' for detailed help on a specific builtin.");
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
            println!(
                "{{\"name\":\"{}\",\"user_defined\":true,\"params\":{}}}",
                rsig.name,
                rsig.params.len()
            );
        } else {
            print!("{}", rsig.render_help());
        }
        return 0;
    }

    if let Some(sig) = crate::signature::SIGNATURES.get(cmd) {
        if as_record {
            // Print the to_record() form as JSON for downstream consumption.
            let json = sig.to_record().to_json();
            match serde_json::to_string_pretty(&json) {
                Ok(s) => println!("{}", s),
                Err(_) => println!("{:?}", sig),
            }
        } else {
            print!("{}", sig.render_help());
        }
        return 0;
    }

    // Fall back to legacy short-form HELP_ENTRIES.
    for (name, desc) in HELP_ENTRIES {
        if *name == cmd {
            println!("{}", desc);
            return 0;
        }
    }
    eprintln!("jsh: help: no help for '{}'", cmd);
    1
}
