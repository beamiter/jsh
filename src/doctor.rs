//! Read-only diagnostics for the environment around jsh.
//!
//! `jsh doctor` deliberately does not source startup files, contact an AI
//! provider, start helper programs, or create persistence files. It reports
//! whether those boundaries appear usable and gives JSON consumers the same
//! information without ever including credential values.

#[cfg(feature = "ai")]
use crate::agent::AgentProtocolConfigError;
#[cfg(feature = "ai")]
use crate::ai::{AiConfig, AiProvider};
use serde::Serialize;
use std::ffi::CString;
use std::io::{self, IsTerminal, Write};
use std::os::unix::ffi::OsStrExt;
use std::os::unix::fs::MetadataExt;
use std::path::{Path, PathBuf};

pub const HELP: &str = concat!(
    "Usage:\n",
    "  jsh doctor [--json] [--strict] [--rcfile FILE]\n\n",
    "Checks:\n",
    "  runtime      Current directory, terminal, and locale\n",
    "  startup      Startup file and Bash compatibility helper\n",
    "  persistence  History, frecency, sessions, and execution state paths\n",
    "  helpers      Trusted Git and desktop-notification helpers\n",
    "  AI           Opt-in state and credential presence (never the credential)\n\n",
    "Options:\n",
    "  --json        Emit a stable machine-readable report\n",
    "  --strict      Exit 1 when the report contains warnings\n",
    "  --rcfile FILE Diagnose FILE instead of the default startup file\n",
    "  -h, --help    Print this help\n\n",
    "The command is read-only and performs no network requests. Without --strict,\n",
    "warnings do not change the exit status. Malformed arguments exit with 2.\n",
);

const STATUS_OK: i32 = 0;
const STATUS_WARNINGS: i32 = 1;
const STATUS_USAGE: i32 = 2;
const STATUS_IO_ERROR: i32 = 74;
const MAX_RENDERED_VALUE_BYTES: usize = 4096;
const MAX_STARTUP_FILE_BYTES: u64 = 8 * 1024 * 1024;

#[derive(Clone, Debug, PartialEq, Eq)]
enum Request {
    Help,
    Diagnose {
        json: bool,
        strict: bool,
        rcfile: Option<PathBuf>,
    },
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
enum Level {
    Pass,
    Info,
    Warn,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
struct Check {
    name: &'static str,
    level: Level,
    message: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    hint: Option<String>,
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize)]
struct Summary {
    passed: usize,
    info: usize,
    warnings: usize,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
struct Report {
    ok: bool,
    schema_version: u32,
    kind: &'static str,
    healthy: bool,
    version: &'static str,
    target: String,
    features: Features,
    summary: Summary,
    checks: Vec<Check>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize)]
struct Features {
    ai: bool,
}

pub fn run_args(args: &[String]) -> i32 {
    let json_hint = args.iter().any(|arg| arg == "--json");
    let request = match parse_args(args) {
        Ok(request) => request,
        Err(message) => {
            let rendered = render_usage_error(&message, json_hint);
            let _ = io::stderr().write_all(rendered.as_bytes());
            return STATUS_USAGE;
        }
    };

    let mut stdout = io::stdout().lock();
    match run_with_writer(request, &mut stdout) {
        Ok(true) => STATUS_WARNINGS,
        Ok(false) => STATUS_OK,
        Err(error) if error.kind() == io::ErrorKind::BrokenPipe => STATUS_OK,
        Err(error) => {
            eprintln!("jsh: doctor: cannot write report: {error}");
            STATUS_IO_ERROR
        }
    }
}

fn parse_args(args: &[String]) -> Result<Request, String> {
    if let [arg] = args {
        if matches!(arg.as_str(), "-h" | "--help" | "help") {
            return Ok(Request::Help);
        }
    }

    let mut json = false;
    let mut strict = false;
    let mut rcfile = None;
    let mut index = 0usize;
    while index < args.len() {
        match args[index].as_str() {
            "--json" if json => return Err("option '--json' may only be used once".to_string()),
            "--json" => json = true,
            "--strict" if strict => {
                return Err("option '--strict' may only be used once".to_string());
            }
            "--strict" => strict = true,
            "--rcfile" => {
                if rcfile.is_some() {
                    return Err("option '--rcfile' may only be used once".to_string());
                }
                let value = args
                    .get(index + 1)
                    .filter(|value| !value.is_empty())
                    .ok_or_else(|| "option '--rcfile' requires a file".to_string())?;
                rcfile = Some(PathBuf::from(value));
                index += 1;
            }
            option if option.starts_with("--rcfile=") => {
                if rcfile.is_some() {
                    return Err("option '--rcfile' may only be used once".to_string());
                }
                let value = option.trim_start_matches("--rcfile=");
                if value.is_empty() {
                    return Err("option '--rcfile' requires a file".to_string());
                }
                rcfile = Some(PathBuf::from(value));
            }
            option => return Err(format!("unknown doctor option '{option}'")),
        }
        index += 1;
    }
    Ok(Request::Diagnose {
        json,
        strict,
        rcfile,
    })
}

/// Return whether strict mode should fail after the report has been written.
fn run_with_writer(request: Request, output: &mut impl Write) -> io::Result<bool> {
    match request {
        Request::Help => {
            output.write_all(HELP.as_bytes())?;
            Ok(false)
        }
        Request::Diagnose {
            json,
            strict,
            rcfile,
        } => {
            let report = diagnose(rcfile.as_deref());
            if json {
                serde_json::to_writer(&mut *output, &report).map_err(io::Error::other)?;
                output.write_all(b"\n")?;
            } else {
                output.write_all(render_human(&report).as_bytes())?;
            }
            Ok(strict && !report.healthy)
        }
    }
}

fn diagnose(rcfile: Option<&Path>) -> Report {
    let mut checks = Vec::new();
    runtime_checks(&mut checks);
    startup_and_persistence_checks(&mut checks, rcfile);
    helper_checks(&mut checks);
    ai_check(&mut checks);

    let mut summary = Summary::default();
    for check in &checks {
        match check.level {
            Level::Pass => summary.passed += 1,
            Level::Info => summary.info += 1,
            Level::Warn => summary.warnings += 1,
        }
    }

    Report {
        ok: true,
        schema_version: 1,
        kind: "doctor",
        healthy: summary.warnings == 0,
        version: env!("CARGO_PKG_VERSION"),
        target: format!("{}-{}", std::env::consts::ARCH, std::env::consts::OS),
        features: Features {
            ai: cfg!(feature = "ai"),
        },
        summary,
        checks,
    }
}

fn runtime_checks(checks: &mut Vec<Check>) {
    match std::env::current_dir() {
        Ok(cwd) => checks.push(pass(
            "runtime.cwd",
            format!("current directory: {}", safe_path(&cwd)),
        )),
        Err(error) => checks.push(warn(
            "runtime.cwd",
            format!("current directory is unavailable: {error}"),
            "enter an existing readable directory before starting jsh",
        )),
    }

    if io::stdin().is_terminal() && io::stdout().is_terminal() {
        checks.push(pass("runtime.terminal", "interactive terminal detected"));
        match std::env::var("TERM") {
            Ok(term) if !term.trim().is_empty() && !term.eq_ignore_ascii_case("dumb") => {
                checks.push(pass(
                    "runtime.term",
                    "TERM advertises terminal capabilities",
                ));
            }
            _ => checks.push(warn(
                "runtime.term",
                "interactive terminal has no usable TERM value",
                "set TERM to the terminal's capability name, such as xterm-256color",
            )),
        }
    } else {
        checks.push(info(
            "runtime.terminal",
            "not attached to an interactive terminal (normal for scripts and CI)",
        ));
    }

    let locale = ["LC_ALL", "LC_CTYPE", "LANG"]
        .into_iter()
        .find_map(|name| std::env::var(name).ok().filter(|value| !value.is_empty()));
    match locale {
        Some(value) if locale_is_utf8(&value) => {
            checks.push(pass("runtime.locale", "UTF-8 locale detected"));
        }
        Some(_) => checks.push(info(
            "runtime.locale",
            "locale does not advertise UTF-8; Unicode editing may depend on the terminal",
        )),
        None => checks.push(info(
            "runtime.locale",
            "no locale variable is set; the platform default will be used",
        )),
    }
}

fn startup_and_persistence_checks(checks: &mut Vec<Check>, rcfile: Option<&Path>) {
    let startup_home = crate::environment::resolve_home_dir();
    let state_home = dirs::home_dir();

    if let Some(raw_real_home) = std::env::var_os("JSH_REAL_HOME").filter(|value| !value.is_empty())
    {
        let configured = PathBuf::from(raw_real_home);
        if configured == startup_home {
            checks.push(pass(
                "startup.real_home",
                format!("JSH_REAL_HOME is active: {}", safe_path(&startup_home)),
            ));
        } else {
            checks.push(warn(
                "startup.real_home",
                "JSH_REAL_HOME is set but is not a usable absolute directory",
                "set it to an existing absolute directory or unset it",
            ));
        }
    }

    checks.push(pass(
        "startup.home",
        format!("startup home: {}", safe_path(&startup_home)),
    ));
    let startup_file = rcfile
        .map(Path::to_path_buf)
        .unwrap_or_else(|| startup_home.join(".bashrc"));
    inspect_startup_file(checks, &startup_file, rcfile.is_some());

    let Some(home) = state_home else {
        checks.push(warn(
            "persistence.home",
            "history, bookmarks, completions, and frecency have no home directory",
            "set HOME to enable persistent interactive state",
        ));
        return;
    };

    check_writable_namespace(
        checks,
        "persistence.home",
        &home,
        "history, bookmarks, completions, frecency, and sessions",
        false,
    );
    check_writable_namespace(
        checks,
        "persistence.sessions",
        &home.join(".jsh/sessions"),
        "session snapshots",
        false,
    );

    let state_dir = dirs::state_dir().unwrap_or_else(|| home.join(".local/state"));
    let journal = configured_journal_path(checks, &state_dir);
    if let Some((path, custom)) = journal.as_ref() {
        if let Some(parent) = path.parent() {
            check_writable_namespace(
                checks,
                "persistence.journal",
                parent,
                "execution context",
                *custom,
            );
        }
    }
    persistence_integrity_checks(
        checks,
        &home,
        journal.as_ref().map(|(path, _)| path.as_path()),
    );
}

fn inspect_startup_file(checks: &mut Vec<Check>, path: &Path, required: bool) {
    match std::fs::symlink_metadata(path) {
        Ok(metadata) if metadata.file_type().is_symlink() && required => checks.push(warn(
            "startup.file",
            format!(
                "{} is a symlink; explicit rcfiles are not followed",
                safe_path(path)
            ),
            "pass the resolved regular-file path to --rcfile",
        )),
        Ok(metadata) if metadata.file_type().is_symlink() => match std::fs::metadata(path) {
            Ok(target)
                if target.is_file()
                    && target.len() <= MAX_STARTUP_FILE_BYTES
                    && path_accessible(path, nix::libc::R_OK) =>
            {
                checks.push(pass(
                    "startup.file",
                    format!(
                        "startup symlink resolves to a bounded file: {}",
                        safe_path(path)
                    ),
                ));
            }
            _ => checks.push(warn(
                "startup.file",
                format!(
                    "{} does not resolve to a usable startup file",
                    safe_path(path)
                ),
                "replace it with a symlink to a regular file no larger than 8 MiB",
            )),
        },
        Ok(metadata) if !metadata.is_file() => checks.push(warn(
            "startup.file",
            format!("{} is not a regular file", safe_path(path)),
            "replace it with a regular startup file",
        )),
        Ok(metadata) if metadata.len() > MAX_STARTUP_FILE_BYTES => checks.push(warn(
            "startup.file",
            format!("{} exceeds the 8 MiB startup limit", safe_path(path)),
            "reduce the startup file or select a smaller file with --rcfile",
        )),
        Ok(_) if !path_accessible(path, nix::libc::R_OK) => checks.push(warn(
            "startup.file",
            format!("{} is not readable", safe_path(path)),
            "fix the startup file and parent-directory permissions",
        )),
        Ok(_) => checks.push(pass(
            "startup.file",
            format!("startup file is readable and bounded: {}", safe_path(path)),
        )),
        Err(error) if error.kind() == io::ErrorKind::NotFound && !required => checks.push(info(
            "startup.file",
            format!("no {} (a startup file is optional)", safe_path(path)),
        )),
        Err(error) if error.kind() == io::ErrorKind::NotFound => checks.push(warn(
            "startup.file",
            format!("requested rcfile does not exist: {}", safe_path(path)),
            "create the file or pass an existing path",
        )),
        Err(error) => checks.push(warn(
            "startup.file",
            format!("cannot inspect {}: {error}", safe_path(path)),
            "check the startup file and parent-directory permissions",
        )),
    }
}

fn configured_journal_path(checks: &mut Vec<Check>, state_dir: &Path) -> Option<(PathBuf, bool)> {
    if std::env::var("JSH_EXECUTION_JOURNAL")
        .ok()
        .is_some_and(|value| falsey(&value))
    {
        checks.push(info(
            "persistence.journal",
            "execution context journal is explicitly disabled",
        ));
        return None;
    }
    journal_path_for_override(
        checks,
        state_dir,
        std::env::var_os("JSH_EXECUTION_JOURNAL_PATH"),
    )
}

fn journal_path_for_override(
    checks: &mut Vec<Check>,
    state_dir: &Path,
    override_path: Option<std::ffi::OsString>,
) -> Option<(PathBuf, bool)> {
    let (path, custom) = match override_path {
        Some(raw) if raw.is_empty() => (state_dir.join("jsh/executions.jsonl"), false),
        Some(raw) => {
            let path = PathBuf::from(raw);
            if !path.is_absolute() {
                checks.push(warn(
                    "persistence.journal",
                    "JSH_EXECUTION_JOURNAL_PATH must be an absolute path",
                    "set an absolute file path or unset the override",
                ));
                return None;
            }
            (path, true)
        }
        None => (state_dir.join("jsh/executions.jsonl"), false),
    };
    if !crate::execution::is_valid_journal_path(&path) {
        checks.push(warn(
            "persistence.journal",
            format!(
                "execution journal path must name a terminal-visible file within {} bytes and must not use the reserved {} sidecar name",
                crate::execution::MAX_JOURNAL_PATH_BYTES,
                crate::execution::JOURNAL_LOCK_FILE_NAME
            ),
            "remove control or invisible formatting, shorten the path, or choose a different file name",
        ));
        return None;
    }
    Some((path, custom))
}

fn persistence_integrity_checks(checks: &mut Vec<Check>, home: &Path, journal: Option<&Path>) {
    let mut candidates = vec![
        home.join(".jsh_history"),
        home.join(".jsh_z"),
        home.join(".jsh_completions"),
        home.join(".jsh_bookmarks"),
    ];
    if let Some(journal) = journal {
        candidates.push(journal.to_path_buf());
        if let Some(parent) = journal.parent() {
            let lock = parent.join(crate::execution::JOURNAL_LOCK_FILE_NAME);
            if lock != journal {
                candidates.push(lock);
            }
        }
    }
    let euid = unsafe { nix::libc::geteuid() };
    let mut present = 0usize;
    let mut safe = 0usize;
    for path in candidates {
        let metadata = match std::fs::symlink_metadata(&path) {
            Ok(metadata) => metadata,
            Err(error) if error.kind() == io::ErrorKind::NotFound => continue,
            Err(error) => {
                checks.push(warn(
                    "persistence.integrity",
                    format!("cannot inspect {}: {error}", safe_path(&path)),
                    "check the containing directory permissions",
                ));
                continue;
            }
        };
        present += 1;
        let issue = if metadata.file_type().is_symlink() {
            Some("is a symlink")
        } else if !metadata.is_file() {
            Some("is not a regular file")
        } else if metadata.uid() != euid {
            Some("is not owned by the current user")
        } else if metadata.nlink() != 1 {
            Some("has multiple hard links")
        } else if metadata.mode() & 0o022 != 0 {
            Some("is writable by another user")
        } else {
            safe += 1;
            None
        };
        if let Some(issue) = issue {
            checks.push(warn(
                "persistence.integrity",
                format!("{} {issue}", safe_path(&path)),
                "move the entry aside and let jsh create a fresh private file",
            ));
        }
    }
    if present == 0 {
        checks.push(info(
            "persistence.integrity",
            "no persistent data files exist yet",
        ));
    } else if safe == present {
        checks.push(pass(
            "persistence.integrity",
            format!("all {safe} existing persistent data file(s) are private"),
        ));
    }
}

fn check_writable_namespace(
    checks: &mut Vec<Check>,
    name: &'static str,
    intended: &Path,
    purpose: &str,
    require_unshared: bool,
) {
    let Some(existing) = nearest_existing_ancestor(intended) else {
        checks.push(warn(
            name,
            format!("no existing parent for {}", safe_path(intended)),
            "create a writable parent directory",
        ));
        return;
    };
    let owned = std::fs::symlink_metadata(&existing).is_ok_and(|metadata| {
        metadata.is_dir()
            && !metadata.file_type().is_symlink()
            && metadata.uid() == unsafe { nix::libc::geteuid() }
            && (!require_unshared || crate::execution::journal_parent_mode_is_safe(metadata.mode()))
    });
    if owned && path_accessible_for_creation(&existing) {
        checks.push(pass(
            name,
            format!("{purpose} can use {}", safe_path(intended)),
        ));
    } else {
        checks.push(warn(
            name,
            format!("{purpose} cannot write below {}", safe_path(&existing)),
            "fix directory ownership/permissions; jsh will continue without that persistent state",
        ));
    }
}

fn helper_checks(checks: &mut Vec<Check>) {
    helper_check(
        checks,
        "bash",
        "helper.bash",
        true,
        "Bash startup-file compatibility",
    );
    helper_check(
        checks,
        "git",
        "helper.git",
        false,
        "Git prompt and completion",
    );
    helper_check(
        checks,
        "notify-send",
        "helper.notify",
        false,
        "desktop job notifications",
    );
}

fn helper_check(
    checks: &mut Vec<Check>,
    helper: &str,
    name: &'static str,
    important: bool,
    purpose: &str,
) {
    let variable = helper_variable(helper);
    let configured = std::env::var_os(&variable).filter(|value| !value.is_empty());
    match crate::io_guard::trusted_helper_quiet(helper) {
        Some(path) => checks.push(pass(
            name,
            format!("{purpose}: trusted helper {}", safe_path(&path)),
        )),
        None if configured.is_some() => checks.push(warn(
            name,
            format!("{variable} does not name a trusted executable"),
            "use an absolute executable path whose resolved directory chain is not replaceable by other users",
        )),
        None if important => checks.push(warn(
            name,
            format!("{purpose} is unavailable"),
            format!("set {variable} to a trusted absolute path"),
        )),
        None => checks.push(info(
            name,
            format!("{purpose} is unavailable (optional)"),
        )),
    }
}

#[cfg(feature = "ai")]
fn ai_check(checks: &mut Vec<Check>) {
    let opted_in = std::env::var("JSH_AI_PROVIDER").is_ok_and(|value| !value.trim().is_empty())
        || std::env::var("JSH_AI_ENABLED").is_ok_and(|value| truthy(&value));
    match AiConfig::from_env() {
        Some(config) => {
            let provider = match config.provider {
                AiProvider::OpenAI => "OpenAI",
                AiProvider::Anthropic => "Anthropic",
                AiProvider::Ollama => "Ollama",
            };
            agent_protocol_check(checks, &config);
            if crate::ai::validate_config(&config).is_err() {
                checks.push(warn(
                    "ai.configuration",
                    format!(
                        "{provider} is enabled but its model, endpoint, or credential is invalid"
                    ),
                    "check JSH_AI_MODEL, JSH_AI_BASE_URL, and the provider credential",
                ));
                return;
            }
            let cloud_without_key = config.provider != AiProvider::Ollama
                && config
                    .api_key
                    .as_deref()
                    .is_none_or(|key| key.trim().is_empty());
            if cloud_without_key {
                checks.push(warn(
                    "ai.configuration",
                    format!("{provider} is enabled but no API credential is present"),
                    "set the provider-specific API key or JSH_AI_API_KEY",
                ));
            } else if config.provider == AiProvider::Ollama {
                checks.push(pass(
                    "ai.configuration",
                    "Ollama is enabled as a local provider; extended context remains local",
                ));
            } else {
                let context = if config.allows_extended_context() {
                    "extended context allowed"
                } else {
                    "extended context disabled"
                };
                checks.push(pass(
                    "ai.configuration",
                    format!("{provider} is enabled; credential presence verified; {context}"),
                ));
            }
        }
        None if opted_in => checks.push(warn(
            "ai.configuration",
            "AI was enabled but the provider configuration is invalid",
            "use JSH_AI_PROVIDER=openai, anthropic, or ollama",
        )),
        None => checks.push(info(
            "ai.configuration",
            "AI is disabled until JSH_AI_PROVIDER or JSH_AI_ENABLED explicitly opts in",
        )),
    }
}

#[cfg(feature = "ai")]
fn agent_protocol_check(checks: &mut Vec<Check>, config: &AiConfig) {
    let provider = config.chat_config(1, None).provider;
    match crate::agent::configured_agent_protocol_from_env(provider) {
        Ok(protocol) => {
            let peer = if std::env::var_os("JSH_AGENT_PEER_CAPABILITIES").is_some() {
                "advertised peer"
            } else {
                "legacy text-compatible peer"
            };
            checks.push(pass(
                "ai.agent_protocol",
                format!(
                    "Agent protocol '{}' is supported for complete delivery with the {peer}",
                    protocol.as_wire_name()
                ),
            ));
        }
        Err(error) => {
            let category = match error {
                AgentProtocolConfigError::InvalidProtocol => "invalid",
                AgentProtocolConfigError::InvalidPeer(_) => "malformed or unsupported",
                AgentProtocolConfigError::UnsupportedSelection(_) => "unsupported",
            };
            checks.push(warn(
                "ai.agent_protocol",
                format!("Agent protocol negotiation is {category}: {error}"),
                "use a canonical bounded JSH_AGENT_PEER_CAPABILITIES token and select text or native-tools; omit both variables for legacy text compatibility",
            ));
        }
    }
}

#[cfg(not(feature = "ai"))]
fn ai_check(checks: &mut Vec<Check>) {
    checks.push(info(
        "ai.configuration",
        "AI support was not compiled into this binary",
    ));
}

fn nearest_existing_ancestor(path: &Path) -> Option<PathBuf> {
    let mut current = path.to_path_buf();
    loop {
        match std::fs::symlink_metadata(&current) {
            Ok(metadata) if metadata.is_dir() || metadata.file_type().is_symlink() => {
                return Some(current);
            }
            Ok(_) => return current.parent().map(Path::to_path_buf),
            Err(_) => {
                if !current.pop() {
                    return None;
                }
            }
        }
    }
}

fn path_accessible_for_creation(path: &Path) -> bool {
    path_accessible(path, nix::libc::W_OK | nix::libc::X_OK)
}

fn path_accessible(path: &Path, mode: i32) -> bool {
    let Ok(encoded) = CString::new(path.as_os_str().as_bytes()) else {
        return false;
    };
    // access(2) is intentionally non-mutating and checks the real identity.
    unsafe { nix::libc::access(encoded.as_ptr(), mode) == 0 }
}

fn helper_variable(name: &str) -> String {
    let suffix: String = name
        .bytes()
        .map(|byte| match byte {
            b'-' => '_',
            other => other.to_ascii_uppercase() as char,
        })
        .collect();
    format!("JSH_HELPER_{suffix}")
}

fn safe_path(path: &Path) -> String {
    crate::terminal_text::escape_inline(&path.to_string_lossy(), MAX_RENDERED_VALUE_BYTES)
}

fn locale_is_utf8(value: &str) -> bool {
    let normalized = value.to_ascii_lowercase().replace('-', "");
    normalized.contains("utf8")
}

fn truthy(value: &str) -> bool {
    matches!(
        value.trim().to_ascii_lowercase().as_str(),
        "1" | "true" | "yes" | "on"
    )
}

fn falsey(value: &str) -> bool {
    matches!(
        value.trim().to_ascii_lowercase().as_str(),
        "" | "0" | "false" | "no" | "off"
    )
}

fn pass(name: &'static str, message: impl Into<String>) -> Check {
    Check {
        name,
        level: Level::Pass,
        message: message.into(),
        hint: None,
    }
}

fn info(name: &'static str, message: impl Into<String>) -> Check {
    Check {
        name,
        level: Level::Info,
        message: message.into(),
        hint: None,
    }
}

fn warn(name: &'static str, message: impl Into<String>, hint: impl Into<String>) -> Check {
    Check {
        name,
        level: Level::Warn,
        message: message.into(),
        hint: Some(hint.into()),
    }
}

fn render_human(report: &Report) -> String {
    let mut output = format!(
        "jsh doctor {} ({})\nAI feature: {}\n\n",
        report.version,
        report.target,
        if report.features.ai {
            "enabled"
        } else {
            "disabled"
        }
    );
    for check in &report.checks {
        let marker = match check.level {
            Level::Pass => "[ok]",
            Level::Info => "[--]",
            Level::Warn => "[!!]",
        };
        output.push_str(&format!("{marker} {:<22} {}\n", check.name, check.message));
        if let Some(hint) = &check.hint {
            output.push_str(&format!("     hint: {hint}\n"));
        }
    }
    output.push_str(&format!(
        "\nSummary: {} passed, {} informational, {} warning(s)\n",
        report.summary.passed, report.summary.info, report.summary.warnings
    ));
    output
}

fn render_usage_error(message: &str, json: bool) -> String {
    if json {
        return format!(
            "{}\n",
            serde_json::json!({
                "ok": false,
                "schema_version": 1,
                "error": { "kind": "usage", "message": message },
            })
        );
    }
    format!("jsh: doctor: {message}\nTry 'jsh doctor --help' for more information.\n")
}

#[cfg(test)]
mod tests {
    use super::*;

    fn strings(values: &[&str]) -> Vec<String> {
        values.iter().map(|value| (*value).to_string()).collect()
    }

    #[test]
    fn parses_only_the_small_documented_surface() {
        assert_eq!(
            parse_args(&[]).unwrap(),
            Request::Diagnose {
                json: false,
                strict: false,
                rcfile: None,
            }
        );
        assert_eq!(
            parse_args(&strings(&["--strict", "--rcfile=custom.jsh", "--json"])).unwrap(),
            Request::Diagnose {
                json: true,
                strict: true,
                rcfile: Some(PathBuf::from("custom.jsh")),
            }
        );
        assert_eq!(parse_args(&strings(&["--help"])).unwrap(), Request::Help);
        assert!(parse_args(&strings(&["--json", "--json"])).is_err());
        assert!(parse_args(&strings(&["--strict", "--strict"])).is_err());
        assert!(parse_args(&strings(&["--rcfile="])).is_err());
        assert!(parse_args(&strings(&["--rcfile", "one", "--rcfile=two"])).is_err());
        assert!(parse_args(&strings(&["--verbose"])).is_err());
    }

    #[test]
    fn json_report_has_a_stable_envelope_and_consistent_counts() {
        let report = diagnose(None);
        let encoded = serde_json::to_value(&report).unwrap();
        assert_eq!(encoded["ok"], true);
        assert_eq!(encoded["schema_version"], 1);
        assert_eq!(encoded["kind"], "doctor");
        assert!(encoded["checks"].as_array().unwrap().len() >= 8);
        assert_eq!(
            report.summary.passed + report.summary.info + report.summary.warnings,
            report.checks.len()
        );
    }

    #[test]
    fn human_output_does_not_emit_terminal_controls_from_paths() {
        let report = Report {
            ok: true,
            schema_version: 1,
            kind: "doctor",
            healthy: true,
            version: "test",
            target: "test".to_string(),
            features: Features { ai: true },
            summary: Summary {
                passed: 1,
                info: 0,
                warnings: 0,
            },
            checks: vec![pass(
                "runtime.cwd",
                format!(
                    "path: {}",
                    crate::terminal_text::escape_inline("/tmp/\x1b]0;bad\x07", 128)
                ),
            )],
        };
        let output = render_human(&report);
        assert!(!output.contains('\x1b'));
        assert!(!output.contains('\x07'));
        assert!(output.contains("\\x1b]0;bad\\x07"));
    }

    #[test]
    fn empty_journal_path_override_uses_the_runtime_default() {
        let state_dir = Path::new("/tmp/jsh-doctor-state");
        let mut checks = Vec::new();

        let path =
            journal_path_for_override(&mut checks, state_dir, Some(std::ffi::OsString::new()));

        assert_eq!(path, Some((state_dir.join("jsh/executions.jsonl"), false)));
        assert!(checks.is_empty());
    }

    #[test]
    fn journal_path_diagnostics_match_the_runtime_safety_boundary() {
        let state_dir = Path::new("/tmp/jsh-doctor-state");
        for unsafe_path in [
            "/",
            "/tmp/bad\nname.jsonl",
            "/tmp/bad\u{0080}name.jsonl",
            "/tmp/bad\u{202e}name.jsonl",
            "/tmp/bad\u{fff9}name.jsonl",
            "/tmp/executions.lock",
            "/tmp/EXECUTIONS.LOCK",
        ] {
            let mut checks = Vec::new();
            assert!(
                journal_path_for_override(&mut checks, state_dir, Some(unsafe_path.into()))
                    .is_none(),
                "accepted {unsafe_path:?}"
            );
            assert_eq!(checks.len(), 1);
            assert_eq!(checks[0].level, Level::Warn);
        }
    }

    #[test]
    fn custom_journal_diagnostics_reject_a_shared_writable_parent() {
        use std::os::unix::fs::PermissionsExt;

        let dir = tempfile::tempdir().unwrap();
        std::fs::set_permissions(dir.path(), std::fs::Permissions::from_mode(0o770)).unwrap();

        let mut checks = Vec::new();
        check_writable_namespace(
            &mut checks,
            "persistence.journal",
            dir.path(),
            "execution context",
            true,
        );
        assert_eq!(checks[0].level, Level::Warn);

        checks.clear();
        check_writable_namespace(
            &mut checks,
            "persistence.journal",
            dir.path(),
            "execution context",
            false,
        );
        assert_eq!(checks[0].level, Level::Pass);
    }

    #[test]
    fn persistence_integrity_includes_the_fixed_journal_lock_sidecar() {
        use std::os::unix::fs::PermissionsExt;

        let dir = tempfile::tempdir().unwrap();
        std::fs::set_permissions(dir.path(), std::fs::Permissions::from_mode(0o700)).unwrap();
        let journal = dir.path().join("events.jsonl");
        let lock = dir.path().join("executions.lock");
        std::fs::write(&journal, b"").unwrap();
        std::fs::set_permissions(&journal, std::fs::Permissions::from_mode(0o600)).unwrap();
        std::fs::write(&lock, b"").unwrap();
        std::fs::set_permissions(&lock, std::fs::Permissions::from_mode(0o622)).unwrap();

        let mut checks = Vec::new();
        persistence_integrity_checks(
            &mut checks,
            Path::new("/definitely/missing/jsh-doctor-home"),
            Some(&journal),
        );

        assert!(checks.iter().any(|check| {
            check.level == Level::Warn && check.message.contains("executions.lock")
        }));
    }

    #[test]
    fn usage_errors_can_be_consumed_as_json() {
        let rendered = render_usage_error("bad option", true);
        let value: serde_json::Value = serde_json::from_str(&rendered).unwrap();
        assert_eq!(value["ok"], false);
        assert_eq!(value["schema_version"], 1);
        assert_eq!(value["error"]["kind"], "usage");
    }

    #[test]
    fn strict_mode_fails_after_rendering_a_warning_report() {
        let mut output = Vec::new();
        let strict_failure = run_with_writer(
            Request::Diagnose {
                json: true,
                strict: true,
                rcfile: Some(PathBuf::from("/definitely/missing/jsh-doctor-rc")),
            },
            &mut output,
        )
        .unwrap();
        assert!(strict_failure);
        let value: serde_json::Value = serde_json::from_slice(&output).unwrap();
        assert_eq!(value["healthy"], false);
        assert!(value["summary"]["warnings"].as_u64().unwrap() >= 1);
    }
}
