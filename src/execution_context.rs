//! Query and render the structured execution journal.
//!
//! Both `jsh context ...` and the interactive `context` builtin use this
//! module, so parsing, status codes, filtering, and JSON schemas stay aligned.

use crate::execution::{ExecutionJournal, ExecutionRecord};
use serde::Serialize;
use std::fmt::Write as _;
use std::io::{self, Write};

pub const DEFAULT_LIST_LIMIT: usize = 20;
pub const MAX_LIST_LIMIT: usize = 2_000;

pub const STATUS_OK: i32 = 0;
pub const STATUS_NOT_FOUND: i32 = 1;
pub const STATUS_USAGE: i32 = 2;
pub const STATUS_IO_ERROR: i32 = 74;
pub const STATUS_DISABLED: i32 = 78;

pub const HELP: &str = concat!(
    "Usage:\n",
    "  context list [-n N] [--session ID] [--json]\n",
    "  context show ID [--json]\n",
    "  context last-failed [--json]\n\n",
    "Commands:\n",
    "  list         List execution summaries in chronological order\n",
    "  show         Show one execution, including captured output\n",
    "  last-failed  Show the most recent failed execution and its output\n\n",
    "Options:\n",
    "  -n N          Return the latest N summaries (default 20, max 2000)\n",
    "  --session ID  Restrict list to one terminal session\n",
    "  --json        Emit an agent-friendly JSON envelope\n\n",
    "Exit status: 0 success, 1 no match, 2 usage, 74 I/O error,\n",
    "             78 journal disabled/unavailable.\n",
);

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum ContextRequest {
    Help,
    List {
        limit: usize,
        session_id: Option<String>,
        json: bool,
    },
    Show {
        id: String,
        json: bool,
    },
    LastFailed {
        json: bool,
    },
}

impl ContextRequest {
    fn json(&self) -> bool {
        match self {
            Self::Help => false,
            Self::List { json, .. } | Self::Show { json, .. } | Self::LastFailed { json } => *json,
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum ContextErrorKind {
    Usage,
    NotFound,
    Io,
    Disabled,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ContextError {
    pub kind: ContextErrorKind,
    pub message: String,
}

impl ContextError {
    fn usage(message: impl Into<String>) -> Self {
        Self {
            kind: ContextErrorKind::Usage,
            message: message.into(),
        }
    }

    fn not_found(message: impl Into<String>) -> Self {
        Self {
            kind: ContextErrorKind::NotFound,
            message: message.into(),
        }
    }

    fn io(error: io::Error, journal: &ExecutionJournal) -> Self {
        Self {
            kind: ContextErrorKind::Io,
            message: format!(
                "cannot read execution journal '{}': {error}",
                journal.path().display()
            ),
        }
    }

    fn disabled() -> Self {
        Self {
            kind: ContextErrorKind::Disabled,
            message: if execution_journal_explicitly_disabled() {
                "execution journal is disabled by JSH_EXECUTION_JOURNAL".to_string()
            } else {
                "execution journal is unavailable because no state directory is configured"
                    .to_string()
            },
        }
    }

    pub fn status(&self) -> i32 {
        match self.kind {
            ContextErrorKind::Usage => STATUS_USAGE,
            ContextErrorKind::NotFound => STATUS_NOT_FOUND,
            ContextErrorKind::Io => STATUS_IO_ERROR,
            ContextErrorKind::Disabled => STATUS_DISABLED,
        }
    }
}

impl std::fmt::Display for ContextError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.message)
    }
}

impl std::error::Error for ContextError {}

/// Parse arguments after the `context` command name.
pub fn parse_args(args: &[String]) -> Result<ContextRequest, ContextError> {
    let Some(command) = args.first().map(String::as_str) else {
        return Err(ContextError::usage(
            "missing context command (expected list, show, or last-failed)",
        ));
    };

    match command {
        "-h" | "--help" | "help" if args.len() == 1 => Ok(ContextRequest::Help),
        "list" => parse_list_args(&args[1..]),
        "show" => parse_show_args(&args[1..]),
        "last-failed" => parse_last_failed_args(&args[1..]),
        other => Err(ContextError::usage(format!(
            "unknown context command '{other}' (expected list, show, or last-failed)"
        ))),
    }
}

fn parse_list_args(args: &[String]) -> Result<ContextRequest, ContextError> {
    let mut limit = DEFAULT_LIST_LIMIT;
    let mut saw_limit = false;
    let mut session_id = None;
    let mut json = false;
    let mut index = 0;
    while index < args.len() {
        match args[index].as_str() {
            "-n" => {
                if saw_limit {
                    return Err(ContextError::usage("option '-n' may only be used once"));
                }
                let raw = args
                    .get(index + 1)
                    .ok_or_else(|| ContextError::usage("option '-n' requires a positive number"))?;
                limit = raw
                    .parse::<usize>()
                    .map_err(|_| ContextError::usage("option '-n' requires a positive integer"))?;
                if !(1..=MAX_LIST_LIMIT).contains(&limit) {
                    return Err(ContextError::usage(format!(
                        "option '-n' must be between 1 and {MAX_LIST_LIMIT}"
                    )));
                }
                saw_limit = true;
                index += 2;
            }
            "--session" => {
                if session_id.is_some() {
                    return Err(ContextError::usage(
                        "option '--session' may only be used once",
                    ));
                }
                let id = args
                    .get(index + 1)
                    .ok_or_else(|| ContextError::usage("option '--session' requires an ID"))?;
                validate_session_id(id)?;
                session_id = Some(id.clone());
                index += 2;
            }
            "--json" => {
                if json {
                    return Err(ContextError::usage("option '--json' may only be used once"));
                }
                json = true;
                index += 1;
            }
            option => {
                return Err(ContextError::usage(format!(
                    "unknown option or argument '{option}' for 'context list'"
                )));
            }
        }
    }
    Ok(ContextRequest::List {
        limit,
        session_id,
        json,
    })
}

fn parse_show_args(args: &[String]) -> Result<ContextRequest, ContextError> {
    let mut id = None;
    let mut json = false;
    for arg in args {
        match arg.as_str() {
            "--json" => {
                if json {
                    return Err(ContextError::usage("option '--json' may only be used once"));
                }
                json = true;
            }
            option if option.starts_with('-') => {
                return Err(ContextError::usage(format!(
                    "unknown option '{option}' for 'context show'"
                )));
            }
            value if id.is_none() => {
                validate_execution_id(value)?;
                id = Some(value.to_string());
            }
            value => {
                return Err(ContextError::usage(format!(
                    "unexpected argument '{value}' for 'context show'"
                )));
            }
        }
    }
    let id = id.ok_or_else(|| ContextError::usage("'context show' requires an execution ID"))?;
    Ok(ContextRequest::Show { id, json })
}

fn parse_last_failed_args(args: &[String]) -> Result<ContextRequest, ContextError> {
    let mut json = false;
    for arg in args {
        match arg.as_str() {
            "--json" if !json => json = true,
            "--json" => {
                return Err(ContextError::usage("option '--json' may only be used once"));
            }
            other => {
                return Err(ContextError::usage(format!(
                    "unknown option or argument '{other}' for 'context last-failed'"
                )));
            }
        }
    }
    Ok(ContextRequest::LastFailed { json })
}

fn validate_execution_id(id: &str) -> Result<(), ContextError> {
    if !id.is_empty()
        && id.len() <= 192
        && id
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'_' | b'.'))
    {
        Ok(())
    } else {
        Err(ContextError::usage(
            "execution ID must be 1-192 ASCII letters, digits, '-', '_' or '.'",
        ))
    }
}

fn validate_session_id(id: &str) -> Result<(), ContextError> {
    if !id.is_empty()
        && id.len() <= 128
        && id
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'_'))
    {
        Ok(())
    } else {
        Err(ContextError::usage(
            "session ID must be 1-128 ASCII letters, digits, '-' or '_'",
        ))
    }
}

enum QueryResult {
    Help,
    List(Vec<ExecutionRecord>),
    Show(ExecutionRecord),
    LastFailed(ExecutionRecord),
}

fn query(
    journal: Option<&ExecutionJournal>,
    request: &ContextRequest,
) -> Result<QueryResult, ContextError> {
    if matches!(request, ContextRequest::Help) {
        return Ok(QueryResult::Help);
    }
    let journal = journal.ok_or_else(ContextError::disabled)?;
    match request {
        ContextRequest::Help => Ok(QueryResult::Help),
        ContextRequest::List {
            limit, session_id, ..
        } => {
            let records = journal
                .list(session_id.as_deref(), *limit)
                .map_err(|error| ContextError::io(error, journal))?;
            if records.is_empty() {
                let message = session_id.as_deref().map_or_else(
                    || "no execution records found".to_string(),
                    |id| format!("no execution records found for session '{id}'"),
                );
                Err(ContextError::not_found(message))
            } else {
                Ok(QueryResult::List(records))
            }
        }
        ContextRequest::Show { id, .. } => journal
            .show(id)
            .map_err(|error| ContextError::io(error, journal))?
            .map(QueryResult::Show)
            .ok_or_else(|| ContextError::not_found(format!("execution '{id}' was not found"))),
        ContextRequest::LastFailed { .. } => journal
            .last_failed()
            .map_err(|error| ContextError::io(error, journal))?
            .map(QueryResult::LastFailed)
            .ok_or_else(|| ContextError::not_found("no failed execution record was found")),
    }
}

#[derive(Serialize)]
struct ExecutionSummary {
    id: String,
    session_id: Option<String>,
    seq: u64,
    state: &'static str,
    command_preview: String,
    command_truncated: bool,
    cwd: String,
    started_at_ms: u64,
    exit_code: Option<i32>,
    duration_ms: Option<u64>,
    cwd_after: Option<String>,
    ended_at_ms: Option<u64>,
    has_output: bool,
    output_truncated: bool,
    output_total_bytes: Option<u64>,
}

impl ExecutionSummary {
    fn from_record(record: &ExecutionRecord) -> Self {
        let (command_preview, preview_truncated) = single_line_preview(&record.command, 240);
        let (cwd, _) = single_line_preview(&record.cwd, 240);
        let state = match record.exit_code {
            None => "running",
            Some(0) => "succeeded",
            Some(_) => "failed",
        };
        Self {
            id: record.id.clone(),
            session_id: record.session_id.clone(),
            seq: record.seq,
            state,
            command_preview,
            command_truncated: record.command_truncated || preview_truncated,
            cwd,
            started_at_ms: record.started_at_ms,
            exit_code: record.exit_code,
            duration_ms: record.duration_ms,
            cwd_after: record
                .cwd_after
                .as_deref()
                .map(|cwd| single_line_preview(cwd, 240).0),
            ended_at_ms: record.ended_at_ms,
            has_output: record.output.is_some(),
            output_truncated: record
                .output
                .as_ref()
                .is_some_and(|output| output.truncated),
            output_total_bytes: record.output.as_ref().map(|output| output.total_bytes),
        }
    }
}

fn single_line_preview(value: &str, max_chars: usize) -> (String, bool) {
    let mut preview = String::new();
    let mut chars = value.chars();
    for _ in 0..max_chars {
        let Some(ch) = chars.next() else {
            return (preview, false);
        };
        match ch {
            '\n' => preview.push_str("\\n"),
            '\r' => preview.push_str("\\r"),
            '\t' => preview.push_str("\\t"),
            ch if crate::terminal_text::is_terminal_ambiguous(ch) => {
                let _ = write!(preview, "\\u{{{:x}}}", u32::from(ch));
            }
            ch => preview.push(ch),
        }
    }
    let truncated = chars.next().is_some();
    if truncated {
        preview.push('…');
    }
    (preview, truncated)
}

/// Append untrusted journal text without allowing terminal control injection.
/// Newlines remain structural for readable commands/output; every other C0,
/// DEL, and C1 control is rendered visibly. JSON output does not use this
/// helper and therefore retains the exact source strings.
fn append_human_safe(output: &mut String, value: &str) {
    for ch in value.chars() {
        match ch {
            '\n' => output.push('\n'),
            '\t' => output.push_str("\\t"),
            '\r' => output.push_str("\\r"),
            ch if crate::terminal_text::is_terminal_ambiguous(ch) && u32::from(ch) <= 0x7f => {
                let _ = write!(output, "\\x{:02x}", u32::from(ch));
            }
            ch if crate::terminal_text::is_terminal_ambiguous(ch) => {
                let _ = write!(output, "\\u{{{:x}}}", u32::from(ch));
            }
            ch => output.push(ch),
        }
    }
}

fn render(result: QueryResult, json: bool) -> Result<String, serde_json::Error> {
    if json {
        let value = match result {
            QueryResult::Help => serde_json::json!({
                "ok": true,
                "kind": "help",
                "usage": HELP,
            }),
            QueryResult::List(records) => {
                let executions: Vec<_> =
                    records.iter().map(ExecutionSummary::from_record).collect();
                serde_json::json!({
                    "ok": true,
                    "kind": "list",
                    "count": executions.len(),
                    "executions": executions,
                })
            }
            QueryResult::Show(record) => serde_json::json!({
                "ok": true,
                "kind": "show",
                "execution": record,
            }),
            QueryResult::LastFailed(record) => serde_json::json!({
                "ok": true,
                "kind": "last_failed",
                "execution": record,
            }),
        };
        let mut output = serde_json::to_string(&value)?;
        output.push('\n');
        return Ok(output);
    }

    let mut output = String::new();
    match result {
        QueryResult::Help => output.push_str(HELP),
        QueryResult::List(records) => {
            for record in &records {
                let summary = ExecutionSummary::from_record(record);
                let exit = summary
                    .exit_code
                    .map_or_else(|| "-".to_string(), |code| code.to_string());
                let duration = summary
                    .duration_ms
                    .map_or_else(|| "-".to_string(), |duration| format!("{duration}ms"));
                let session = summary.session_id.as_deref().unwrap_or("-");
                let _ = writeln!(
                    output,
                    "{}  {}  exit={}  duration={}  session={}  {}",
                    summary.id,
                    summary.state,
                    exit,
                    duration,
                    single_line_preview(session, 80).0,
                    summary.command_preview
                );
            }
        }
        QueryResult::Show(record) | QueryResult::LastFailed(record) => {
            render_record_human(&mut output, &record);
        }
    }
    Ok(output)
}

fn render_record_human(output: &mut String, record: &ExecutionRecord) {
    let _ = writeln!(output, "ID: {}", record.id);
    let _ = writeln!(
        output,
        "Session: {}",
        record.session_id.as_deref().unwrap_or("-")
    );
    let _ = writeln!(output, "Sequence: {}", record.seq);
    let _ = writeln!(output, "Started: {} ms", record.started_at_ms);
    let _ = writeln!(
        output,
        "Exit: {}",
        record
            .exit_code
            .map_or_else(|| "running".to_string(), |code| code.to_string())
    );
    let _ = writeln!(
        output,
        "Duration: {}",
        record
            .duration_ms
            .map_or_else(|| "-".to_string(), |duration| format!("{duration} ms"))
    );
    let _ = writeln!(output, "CWD: {}", single_line_preview(&record.cwd, 1_024).0);
    if let Some(cwd_after) = &record.cwd_after {
        let _ = writeln!(
            output,
            "CWD after: {}",
            single_line_preview(cwd_after, 1_024).0
        );
    }
    output.push_str("Command:\n");
    append_human_safe(output, &record.command);
    if !record.command.ends_with('\n') {
        output.push('\n');
    }
    if record.command_truncated {
        output.push_str("[command was truncated in the journal]\n");
    }

    match &record.output {
        None => output.push_str("Output: not captured\n"),
        Some(captured) => {
            let _ = writeln!(
                output,
                "Output: {} captured bytes / {} total bytes{}",
                captured.text.len(),
                captured.total_bytes,
                if captured.truncated {
                    " (truncated)"
                } else {
                    ""
                }
            );
            output.push_str("--- output ---\n");
            append_human_safe(output, &captured.text);
            if !captured.text.ends_with('\n') {
                output.push('\n');
            }
            output.push_str("--- end output ---\n");
        }
    }
}

fn render_error(error: &ContextError, json: bool) -> String {
    if json {
        let mut output = serde_json::json!({
            "ok": false,
            "error": {
                "kind": error.kind,
                "message": error.message,
                "status": error.status(),
            }
        })
        .to_string();
        output.push('\n');
        output
    } else {
        format!("jsh: context: {}\n", error.message)
    }
}

fn execution_journal_explicitly_disabled() -> bool {
    std::env::var("JSH_EXECUTION_JOURNAL")
        .ok()
        .is_some_and(|value| {
            matches!(
                value.trim().to_ascii_lowercase().as_str(),
                "" | "0" | "off" | "false" | "no"
            )
        })
}

/// Parse and execute context arguments. This is the shared CLI/builtin entry.
pub fn run_args(args: &[String]) -> i32 {
    let json_hint = args.iter().any(|arg| arg == "--json");
    let request = match parse_args(args) {
        Ok(request) => request,
        Err(error) => {
            let _ = io::stderr().write_all(render_error(&error, json_hint).as_bytes());
            return error.status();
        }
    };
    run(request)
}

pub fn run(request: ContextRequest) -> i32 {
    let journal = ExecutionJournal::configured();
    let mut stdout = io::stdout().lock();
    let mut stderr = io::stderr().lock();
    run_with_writers(request, journal.as_ref(), &mut stdout, &mut stderr)
}

fn run_with_writers(
    request: ContextRequest,
    journal: Option<&ExecutionJournal>,
    stdout: &mut impl Write,
    stderr: &mut impl Write,
) -> i32 {
    let json = request.json();
    let result = match query(journal, &request) {
        Ok(result) => result,
        Err(error) => {
            let _ = stderr.write_all(render_error(&error, json).as_bytes());
            return error.status();
        }
    };
    let rendered = match render(result, json) {
        Ok(rendered) => rendered,
        Err(error) => {
            let error = ContextError {
                kind: ContextErrorKind::Io,
                message: format!("cannot encode context response: {error}"),
            };
            let _ = stderr.write_all(render_error(&error, json).as_bytes());
            return error.status();
        }
    };
    match stdout.write_all(rendered.as_bytes()) {
        Ok(()) => STATUS_OK,
        Err(error) if error.kind() == io::ErrorKind::BrokenPipe => STATUS_OK,
        Err(error) => {
            let error = ContextError {
                kind: ContextErrorKind::Io,
                message: format!("cannot write context response: {error}"),
            };
            let _ = stderr.write_all(render_error(&error, json).as_bytes());
            error.status()
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::execution::{ExecutionOutput, ExecutionRecord};

    fn strings(values: &[&str]) -> Vec<String> {
        values.iter().map(|value| (*value).to_string()).collect()
    }

    fn record(exit_code: Option<i32>, output: Option<&str>) -> ExecutionRecord {
        ExecutionRecord {
            id: "jsh-test-1".to_string(),
            session_id: Some("tab-1".to_string()),
            seq: 7,
            command: "printf 'secret output'\nnext".to_string(),
            command_truncated: false,
            cwd: "/tmp/project".to_string(),
            started_at_ms: 100,
            exit_code,
            duration_ms: Some(25),
            cwd_after: Some("/tmp/project".to_string()),
            ended_at_ms: Some(125),
            output: output.map(|text| ExecutionOutput {
                text: text.to_string(),
                truncated: false,
                total_bytes: text.len() as u64,
                captured_at_ms: 126,
            }),
        }
    }

    #[test]
    fn parses_all_context_forms_and_option_orders() {
        assert_eq!(
            parse_args(&strings(&[
                "list",
                "--json",
                "--session",
                "tab-1",
                "-n",
                "7"
            ]))
            .unwrap(),
            ContextRequest::List {
                limit: 7,
                session_id: Some("tab-1".to_string()),
                json: true,
            }
        );
        assert_eq!(
            parse_args(&strings(&["show", "--json", "jsh-test.1"])).unwrap(),
            ContextRequest::Show {
                id: "jsh-test.1".to_string(),
                json: true,
            }
        );
        assert_eq!(
            parse_args(&strings(&["last-failed", "--json"])).unwrap(),
            ContextRequest::LastFailed { json: true }
        );
        assert_eq!(
            parse_args(&strings(&["--help"])).unwrap(),
            ContextRequest::Help
        );
    }

    #[test]
    fn rejects_missing_invalid_and_duplicate_arguments() {
        for args in [
            strings(&[]),
            strings(&["unknown"]),
            strings(&["list", "-n", "0"]),
            strings(&["list", "-n", "2001"]),
            strings(&["list", "--session", "../bad"]),
            strings(&["list", "--json", "--json"]),
            strings(&["show"]),
            strings(&["show", "bad/id"]),
            strings(&["show", "one", "two"]),
            strings(&["last-failed", "extra"]),
        ] {
            let error = parse_args(&args).expect_err("arguments must be rejected");
            assert_eq!(error.status(), STATUS_USAGE, "args: {args:?}");
        }
    }

    #[test]
    fn list_json_is_summary_only_while_show_contains_real_output() {
        let list = render(
            QueryResult::List(vec![record(Some(3), Some("SECRET\n"))]),
            true,
        )
        .unwrap();
        let parsed: serde_json::Value = serde_json::from_str(&list).unwrap();
        assert_eq!(parsed["kind"], "list");
        assert_eq!(parsed["executions"][0]["has_output"], true);
        assert!(parsed["executions"][0].get("output").is_none());
        assert!(!list.contains("SECRET"));

        let show = render(QueryResult::Show(record(Some(3), Some("SECRET\n"))), true).unwrap();
        let parsed: serde_json::Value = serde_json::from_str(&show).unwrap();
        assert_eq!(parsed["kind"], "show");
        assert_eq!(parsed["execution"]["output"]["text"], "SECRET\n");
    }

    #[test]
    fn human_show_includes_command_status_and_output() {
        let output = render(
            QueryResult::LastFailed(record(Some(9), Some("compiler failed\n"))),
            false,
        )
        .unwrap();
        assert!(output.contains("ID: jsh-test-1"));
        assert!(output.contains("Exit: 9"));
        assert!(output.contains("printf 'secret output'"));
        assert!(output.contains("compiler failed\n"));
    }

    #[test]
    fn human_show_escapes_terminal_controls_but_json_remains_exact() {
        let control = "before\x1b]133;A\x07after\nnext\u{0085}\u{202e}\u{200b}";
        let mut unsafe_record = record(Some(1), Some(control));
        unsafe_record.command = control.to_string();

        let human = render(QueryResult::Show(unsafe_record.clone()), false).unwrap();
        assert!(!human.contains('\x1b'));
        assert!(!human.contains('\x07'));
        assert!(!human.contains('\u{0085}'));
        assert!(!human.contains('\u{202e}'));
        assert!(!human.contains('\u{200b}'));
        assert!(human.contains("before\\x1b]133;A\\x07after\nnext\\u{85}\\u{202e}\\u{200b}"));

        let json = render(QueryResult::Show(unsafe_record), true).unwrap();
        let parsed: serde_json::Value = serde_json::from_str(&json).unwrap();
        assert_eq!(parsed["execution"]["command"], control);
        assert_eq!(parsed["execution"]["output"]["text"], control);
    }

    #[test]
    fn configured_failures_have_distinct_machine_statuses() {
        let mut stdout = Vec::new();
        let mut stderr = Vec::new();
        let status = run_with_writers(
            ContextRequest::List {
                limit: 20,
                session_id: None,
                json: true,
            },
            None,
            &mut stdout,
            &mut stderr,
        );
        assert_eq!(status, STATUS_DISABLED);
        let parsed: serde_json::Value = serde_json::from_slice(&stderr).unwrap();
        assert_eq!(parsed["error"]["kind"], "disabled");

        let temp = tempfile::tempdir().unwrap();
        let journal = ExecutionJournal::with_path(temp.path().join("empty.jsonl"));
        stdout.clear();
        stderr.clear();
        let status = run_with_writers(
            ContextRequest::LastFailed { json: false },
            Some(&journal),
            &mut stdout,
            &mut stderr,
        );
        assert_eq!(status, STATUS_NOT_FOUND);
        assert!(String::from_utf8(stderr)
            .unwrap()
            .contains("no failed execution"));
    }

    #[test]
    fn journal_io_failures_return_ex_ioerr() {
        let temp = tempfile::tempdir().unwrap();
        let journal_path = temp.path().join("journal-is-a-directory");
        std::fs::create_dir(&journal_path).unwrap();
        let journal = ExecutionJournal::with_path(journal_path);
        let mut stdout = Vec::new();
        let mut stderr = Vec::new();
        let status = run_with_writers(
            ContextRequest::List {
                limit: 20,
                session_id: None,
                json: false,
            },
            Some(&journal),
            &mut stdout,
            &mut stderr,
        );
        assert_eq!(status, STATUS_IO_ERROR);
        assert!(String::from_utf8(stderr)
            .unwrap()
            .contains("cannot read execution journal"));
    }
}
