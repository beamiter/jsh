//! Interactive review-first AI agent built on the shared `jagent` core.
//!
//! The model may only propose commands; every proposal goes through an
//! explicit approval prompt before jsh executes it. Approved commands run in a
//! fresh, one-shot jsh child through the normal parser/executor, with
//! stdout+stderr teed to the terminal and captured as the bounded observation
//! for the next model turn.
//!
//! Configuration reuses the `JSH_AI_*` environment contract from `crate::ai`,
//! plus:
//! - `JSH_AGENT_MAX_TURNS` — model-turn budget (default 16)
//! - `JSH_AGENT_AUTO_APPROVE_READONLY` — retired compatibility switch; when
//!   set, jsh warns and continues to require explicit approval

use crate::ai::AiConfig;
use crate::environment::ShellState;
use jagent::provider::{ChatConfig, HttpRequest, Message, Provider};
use jagent::{
    prepare_agent_request, AgentProtocol, AgentRequestSpec, AgentResponse, AgentSession,
    AgentState, ApprovedCommand, EnvironmentMeta, GitMeta, ModelOutcome, Role, SessionError,
};
use std::fs::{self, File, OpenOptions};
use std::io::{IsTerminal, Read, Write};
use std::os::unix::fs::{DirBuilderExt, OpenOptionsExt, PermissionsExt};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};

const DEFAULT_MAX_TURNS: u32 = 16;
const AGENT_MAX_TOKENS: u32 = 1024;
/// Provider envelopes contain metadata around jagent's 256 KiB assistant-text
/// ceiling. One MiB leaves ample framing room without accepting ureq's much
/// larger generic body default on this command-execution surface.
const MAX_AGENT_RESPONSE_BYTES: u64 = 1024 * 1024;
const MAX_AGENT_DISPLAY_BYTES: usize = 16 * 1024;
/// A validated jagent command is at most 16 KiB. Escaping every ambiguous
/// Unicode format character can expand it, so keep a separate lossless review
/// budget rather than truncating the exact command the user must approve.
const MAX_AGENT_COMMAND_DISPLAY_BYTES: usize = 64 * 1024;
/// Collect at most this much execution output; `AgentSession::observe` samples
/// it further down to its own observation budget.
const MAX_CAPTURED_OUTPUT_BYTES: usize = 128 * 1024;
/// Once the direct jsh child has exited, only pipe-buffered output belongs to
/// that execution. A detached descendant may keep or continuously write the
/// inherited descriptor; cap the final drain so it cannot pin the Agent loop.
const MAX_POST_EXIT_DRAIN_BYTES: usize = 1024 * 1024;
const MAX_CONSECUTIVE_PROTOCOL_RETRIES: u32 = 2;
const MAX_AGENT_SESSION_TURNS: u32 = 1_000;
const INTERNAL_AGENT_CHILD_FLAG: &str = "--jsh-internal-agent-child";
const AGENT_CHILD_SESSION_ID: &str = "agent-child";
const AGENT_CHILD_STATE_DIR_ENV: &str = "JSH_AGENT_CHILD_STATE_DIR";
const AGENT_CHILD_CWD_ENV: &str = "JSH_AGENT_CHILD_CWD";
const AGENT_CHILD_REPORT_ENV: &str = "JSH_AGENT_CHILD_REPORT";
const AGENT_CHILD_COMMAND_ENV: &str = "JSH_AGENT_CHILD_COMMAND";
const AGENT_CHILD_CLAIM_DIR: &str = "claimed";
static AGENT_CHILD_COUNTER: AtomicU64 = AtomicU64::new(0);
/// Argument that turns this binary into a one-shot HTTP transport for its own
/// parent. See [`model_request`].
const INTERNAL_MODEL_REQUEST_FLAG: &str = "--jsh-internal-model-request";
/// Ceiling on the request envelope the parent writes to the child's stdin.
/// A chat request carries the system prompt, the transcript and the observation
/// sample, all of which jagent already bounds; this is the framing backstop.
const MAX_MODEL_REQUEST_ENVELOPE_BYTES: usize = 8 * 1024 * 1024;
/// Wall clock for one model request, including process startup. Comfortably
/// above the transport's own read timeout so the deadline is the outer bound
/// rather than the usual one.
const MODEL_REQUEST_DEADLINE: std::time::Duration = std::time::Duration::from_secs(180);
/// The child answers on stdout with one status byte and then the payload, so a
/// provider error is distinguishable from a transport error without parsing
/// prose. stderr stays empty and is only captured to keep a stuck child from
/// blocking on a full pipe.
const MODEL_CHILD_OK: u8 = b'+';
const MODEL_CHILD_ERR: u8 = b'-';
const MAX_MODEL_CHILD_STDERR_BYTES: usize = 8 * 1024;

/// Create the Agent capture pipe with close-on-exec set on both original
/// descriptors. Linux and Android can do this atomically. Other Unix targets
/// (notably macOS, which has no `pipe2` in nix) set the descriptor flag before
/// either end is exposed to `Command`; owned descriptors close automatically
/// if either `fcntl` call fails.
fn capture_pipe_cloexec() -> nix::Result<(std::os::fd::OwnedFd, std::os::fd::OwnedFd)> {
    #[cfg(any(target_os = "linux", target_os = "android"))]
    {
        nix::unistd::pipe2(nix::fcntl::OFlag::O_CLOEXEC)
    }

    #[cfg(not(any(target_os = "linux", target_os = "android")))]
    {
        use nix::fcntl::{fcntl, FcntlArg, FdFlag};

        let (reader, writer) = nix::unistd::pipe()?;
        fcntl(&reader, FcntlArg::F_SETFD(FdFlag::FD_CLOEXEC))?;
        fcntl(&writer, FcntlArg::F_SETFD(FdFlag::FD_CLOEXEC))?;
        Ok((reader, writer))
    }
}

pub fn builtin_agent(args: &[String], state: &mut ShellState) -> i32 {
    let Some(ai_config) = AiConfig::from_env() else {
        eprintln!(
            "agent: AI is not configured. Set JSH_AI_PROVIDER=anthropic|openai|ollama \
             (plus the provider API key) or JSH_AI_ENABLED=1; see README."
        );
        return 1;
    };
    let chat = chat_config(&ai_config);
    let share_context = ai_config.allows_extended_context();
    let protocol =
        match configured_agent_protocol(std::env::var("JSH_AGENT_PROTOCOL").ok().as_deref()) {
            Ok(protocol) => protocol,
            Err(message) => {
                eprintln!("agent: {message}");
                return 1;
            }
        };

    let goal = if args.is_empty() {
        match read_line("agent goal> ") {
            Some(line) if !line.trim().is_empty() => line,
            _ => return 0,
        }
    } else {
        args.join(" ")
    };

    let mut session = AgentSession::new(max_turns());
    if let Err(error) = session.submit_user(goal) {
        eprintln!("agent: {error}");
        return 1;
    }

    if env_truthy("JSH_AGENT_AUTO_APPROVE_READONLY") {
        eprintln!(
            "agent: JSH_AGENT_AUTO_APPROVE_READONLY is retired; every proposal now requires explicit approval"
        );
    }
    let mut protocol_retries = 0_u32;
    // The agent's working directory persists across its own turns (each
    // approved command starts here, and a command's `cd` carries forward)
    // without ever changing the interactive shell's cwd.
    let mut agent_cwd = std::env::current_dir().unwrap_or_else(|_| PathBuf::from("/"));

    loop {
        if let Some(status) = take_agent_interrupt(&mut session, state) {
            return status;
        }
        match session.state() {
            AgentState::AwaitingModel => {
                status_line(&format!(
                    "thinking… (turn {}/{})",
                    session.turns_used() + 1,
                    session.max_turns()
                ));
                let reply =
                    match request_model(&chat, &session, share_context, &agent_cwd, protocol) {
                        Ok(reply) => reply,
                        Err(error) => {
                            if let Some(status) = take_agent_interrupt(&mut session, state) {
                                return status;
                            }
                            let _ = session.model_failed(&error);
                            let error = crate::ai::redact_sensitive_text(&error);
                            eprintln!(
                                "{}",
                                terminal_safe_message(
                                    "agent: model request failed: ",
                                    &error,
                                    MAX_AGENT_DISPLAY_BYTES
                                )
                            );
                            if session.can_retry_model() && confirm("retry? [y/N] ") {
                                let _ = session.retry_model();
                                continue;
                            }
                            return 1;
                        }
                    };
                if let Some(status) = take_agent_interrupt(&mut session, state) {
                    return status;
                }
                match session.accept_agent_response(&reply) {
                    Ok(ModelOutcome::Proposal {
                        id,
                        command,
                        danger: _,
                    }) => {
                        let danger = jagent::is_dangerous(&command);
                        protocol_retries = 0;
                        let approved = match review_proposal(&mut session, id, &command, danger) {
                            ReviewOutcome::Approved(approved) => approved,
                            ReviewOutcome::Rejected => continue,
                            ReviewOutcome::Insert(command) => {
                                state.pending_editor_insert = Some(command);
                                println!(
                                    "agent: command moved to your next prompt for manual \
                                     review; it was not executed."
                                );
                                return 0;
                            }
                            ReviewOutcome::Quit => return 0,
                        };
                        let (exit_code, output) =
                            run_captured(&approved.command, state, &mut agent_cwd);
                        if let Some(status) = take_agent_interrupt(&mut session, state) {
                            return status;
                        }
                        if let Err(error) =
                            session.observe(approved.proposal_id, exit_code, &output)
                        {
                            eprintln!("agent: {error}");
                            return 1;
                        }
                    }
                    Ok(ModelOutcome::Said(message)) => {
                        protocol_retries = 0;
                        let message = crate::ai::redact_sensitive_text(&message);
                        println!(
                            "{}",
                            terminal_safe_message("agent: ", &message, MAX_AGENT_DISPLAY_BYTES)
                        );
                    }
                    Ok(ModelOutcome::Completed(message)) => {
                        protocol_retries = 0;
                        let message = crate::ai::redact_sensitive_text(&message);
                        println!(
                            "{}",
                            terminal_safe_message(
                                "agent done: ",
                                &message,
                                MAX_AGENT_DISPLAY_BYTES
                            )
                        );
                    }
                    Err(SessionError::Protocol(error)) => {
                        let error = crate::ai::redact_sensitive_text(&error.to_string());
                        eprintln!(
                            "{}",
                            terminal_safe_message(
                                "agent: model reply violated the protocol: ",
                                &error,
                                MAX_AGENT_DISPLAY_BYTES
                            )
                        );
                        if protocol_retries < MAX_CONSECUTIVE_PROTOCOL_RETRIES
                            && session.can_retry_model()
                        {
                            protocol_retries += 1;
                            let _ = session.retry_model();
                        }
                    }
                    Err(error) => {
                        eprintln!("agent: {error}");
                        return 1;
                    }
                }
            }
            AgentState::Ready => {
                let Some(line) = read_line("you> ") else {
                    return 0;
                };
                let line = line.trim().to_string();
                if line.is_empty() || line == "q" || line == "quit" {
                    return 0;
                }
                if let Err(error) = session.submit_user(line) {
                    eprintln!("agent: {error}");
                    if matches!(error, SessionError::TurnLimitReached) {
                        return 1;
                    }
                }
            }
            AgentState::Completed => {
                if !session.can_continue_after_completion() {
                    return 0;
                }
                let Some(line) = read_line("follow-up (Enter to finish)> ") else {
                    return 0;
                };
                let line = line.trim().to_string();
                if line.is_empty() {
                    return 0;
                }
                if session.continue_after_completion().is_err() {
                    return 0;
                }
                if let Err(error) = session.submit_user(line) {
                    eprintln!("agent: {error}");
                    return 1;
                }
            }
            AgentState::TurnLimitReached => {
                eprintln!(
                    "agent: turn budget of {} reached (JSH_AGENT_MAX_TURNS to raise)",
                    session.max_turns()
                );
                return 1;
            }
            AgentState::Cancelled => return 0,
            AgentState::AwaitingApproval { .. } | AgentState::AwaitingObservation { .. } => {
                // Both are resolved inline in the proposal arm; reaching here
                // means an internal bug, so fail instead of spinning.
                eprintln!("agent: internal state error");
                return 1;
            }
        }
    }
}

enum ReviewOutcome {
    Approved(ApprovedCommand),
    Rejected,
    /// Insert-only manual review: the command text goes to the next editor
    /// prompt without executing, and the session records the non-execution.
    Insert(String),
    Quit,
}

fn review_proposal(
    session: &mut AgentSession,
    id: jagent::ProposalId,
    command: &str,
    danger: Option<&'static str>,
) -> ReviewOutcome {
    println!();
    println!(
        "  proposed: {}",
        emphasize(&terminal_safe_inline_text(
            command,
            MAX_AGENT_COMMAND_DISPLAY_BYTES
        ))
    );
    if let Some(reason) = danger {
        println!("  {}", warn(&format!("warning: {reason}")));
    }
    loop {
        let Some(choice) = read_line("  [y] run  [e] edit  [i] insert  [n] reject  [q] quit > ")
        else {
            session.cancel();
            return ReviewOutcome::Quit;
        };
        match choice.trim() {
            "y" | "yes" => {
                let approved = match session.approve(id) {
                    Ok(approved) => approved,
                    Err(error) => {
                        eprintln!("agent: {error}");
                        return ReviewOutcome::Quit;
                    }
                };
                if !confirm_danger(&approved, danger) {
                    // The state machine has already recorded the approval, so
                    // backing out means ending the session rather than
                    // pretending the proposal is pending again.
                    session.cancel();
                    return ReviewOutcome::Quit;
                }
                return ReviewOutcome::Approved(approved);
            }
            "e" | "edit" => {
                let Some(edited) = read_line("  edit> ") else {
                    continue;
                };
                if !crate::terminal_text::is_safe_inline(&edited) {
                    eprintln!(
                        "  agent: edited command contains invisible, bidirectional, or \
                         terminal-control characters"
                    );
                    continue;
                }
                match session.edit_and_approve(id, edited) {
                    Ok(approved) => {
                        let danger = jagent::is_dangerous(&approved.command);
                        if !confirm_danger(&approved, danger) {
                            session.cancel();
                            return ReviewOutcome::Quit;
                        }
                        return ReviewOutcome::Approved(approved);
                    }
                    Err(error) => {
                        eprintln!("  agent: {error}");
                    }
                }
            }
            "i" | "insert" => match session.edit_for_manual_review(id, command) {
                Ok(command) => return ReviewOutcome::Insert(command),
                Err(error) => {
                    eprintln!("  agent: {error}");
                }
            },
            "n" | "no" | "reject" => {
                if let Err(error) = session.reject(id) {
                    eprintln!("agent: {error}");
                    return ReviewOutcome::Quit;
                }
                return ReviewOutcome::Rejected;
            }
            "q" | "quit" => {
                session.cancel();
                return ReviewOutcome::Quit;
            }
            _ => {}
        }
    }
}

/// Recognized-dangerous commands need a second, deliberate confirmation after
/// approval, mirroring jterm4's exact-command confirmation gate.
fn confirm_danger(approved: &ApprovedCommand, danger: Option<&'static str>) -> bool {
    let ambiguous_text = approved
        .command
        .chars()
        .any(crate::terminal_text::is_terminal_ambiguous);
    if danger.is_none() && approved.danger.is_none() && !ambiguous_text {
        return true;
    }
    if let Some(reason) = danger.or(approved.danger) {
        println!("  {}", warn(&format!("dangerous: {reason}")));
    }
    if ambiguous_text {
        println!(
            "  {}",
            warn("dangerous: invisible or bidirectional Unicode is shown as an explicit escape")
        );
    }
    match read_line("  type RUN to execute, anything else aborts > ") {
        Some(line) => line.trim() == "RUN",
        None => false,
    }
}

fn request_model(
    chat: &ChatConfig,
    session: &AgentSession,
    share_context: bool,
    agent_cwd: &Path,
    protocol: AgentProtocol,
) -> Result<AgentResponse, String> {
    let environment = environment_meta(share_context, agent_cwd);
    if crate::signal::pending_status().is_some() {
        return Err("interrupted".to_string());
    }
    // Transcript observations replay real terminal output, which is where API
    // keys and connection strings show up. AgentRequestSpec's secure default
    // redacts every history turn and binds this protocol to its matching
    // system prompt, provider schema, and response decoder.
    let user_text = jagent::agent_user_prompt(
        &session.build_user_prompt_with(protocol),
        &environment,
        None,
    );
    let history = [Message {
        role: Role::User,
        text: user_text,
    }];
    let prepared = prepare_agent_request(chat, AgentRequestSpec::new(&history, protocol))
        .map_err(|error| error.to_string())?;
    debug_assert!(prepared.report.redaction_enabled);
    let raw = model_request(prepared.request.clone(), chat.provider)?;
    prepared
        .parse_response(&raw)
        .map_err(|error| error.to_string())
}

/// Perform one model request in a child process, so cancelling it ends it.
///
/// ureq is intentionally blocking, and a blocking socket read cannot be
/// interrupted in place. Running it on a worker *thread* therefore only ever
/// achieved half the job: INT released the foreground promptly, but the request
/// itself kept running — still connected, still being billed, and still holding
/// the single-flight slot — until the provider's own read timeout expired, and
/// a second request in that window was refused outright.
///
/// A child process has a handle the parent can actually use. `SIGKILL` to the
/// process group ends the request now, which is why there is no in-flight gate
/// here any more: there is never a previous request left to wait for.
///
/// The child is this same binary. That matters: TLS verification, the redirect
/// policy, the response header caps and the body ceiling are all the code in
/// [`perform_model_request`], unchanged and unduplicated. Only *where* it runs
/// moved.
///
/// The envelope travels on stdin rather than argv because it carries the API
/// key, and argv is world-readable through `/proc`.
fn model_request(request: HttpRequest, provider: Provider) -> Result<Vec<u8>, String> {
    if crate::signal::pending_status().is_some() {
        return Err("interrupted".to_string());
    }
    let envelope = encode_model_request(&request, provider);
    if envelope.len() > MAX_MODEL_REQUEST_ENVELOPE_BYTES {
        return Err("model request is too large to send".to_string());
    }

    let executable =
        std::env::current_exe().map_err(|error| format!("could not locate jsh: {error}"))?;
    let mut command = std::process::Command::new(executable);
    command.arg(INTERNAL_MODEL_REQUEST_FLAG);

    let cancelled = || crate::signal::pending_status().is_some();
    let output = crate::io_guard::bounded_command_session(
        &mut command,
        crate::io_guard::BoundedCommand {
            // One status byte of framing on top of the transport's own ceiling.
            stdout_limit: MAX_AGENT_RESPONSE_BYTES as usize + 1,
            stderr_limit: MAX_MODEL_CHILD_STDERR_BYTES,
            timeout: MODEL_REQUEST_DEADLINE,
            stdin: Some(envelope.as_bytes()),
            cancel: Some(&cancelled),
            // A shell that is killed outright must not leave a request running
            // against the provider with nobody to read the answer.
            die_with_parent: true,
            new_session: false,
        },
    );

    let output = match output {
        Ok(output) => output,
        Err(error) if error.kind() == std::io::ErrorKind::Interrupted => {
            return Err("interrupted".to_string())
        }
        Err(error) if error.kind() == std::io::ErrorKind::TimedOut => {
            return Err("model request timed out".to_string())
        }
        Err(error) => return Err(format!("could not run the model request: {error}")),
    };

    match output.stdout.split_first() {
        Some((&MODEL_CHILD_OK, body)) => Ok(body.to_vec()),
        Some((&MODEL_CHILD_ERR, message)) => Err(String::from_utf8_lossy(message).into_owned()),
        // No framing byte at all means the child died before it could answer —
        // a signal, or a binary that is not this jsh. Its stderr is untrusted
        // and is deliberately not echoed.
        _ => Err("the model request did not complete".to_string()),
    }
}

/// Serialize a request for the child. Deliberately jsh's own small envelope
/// rather than serde on jagent's types: the wire format is private to this pair
/// of processes, and it should not move when a dependency adds a field.
fn encode_model_request(request: &HttpRequest, provider: Provider) -> String {
    serde_json::json!({
        "v": 1,
        "provider": provider.as_config_value(),
        "url": request.url,
        "headers": request.headers,
        "body": request.body,
    })
    .to_string()
}

fn decode_model_request(envelope: &str) -> Result<(HttpRequest, Provider), String> {
    let value: serde_json::Value =
        serde_json::from_str(envelope).map_err(|error| format!("malformed request: {error}"))?;
    if value.get("v").and_then(serde_json::Value::as_u64) != Some(1) {
        return Err("unsupported request version".to_string());
    }
    let text = |key: &str| -> Result<String, String> {
        value
            .get(key)
            .and_then(serde_json::Value::as_str)
            .map(str::to_string)
            .ok_or_else(|| format!("request is missing {key}"))
    };
    let provider = match text("provider")?.as_str() {
        "anthropic" => Provider::Anthropic,
        "openai-compatible" => Provider::OpenAiCompatible,
        "ollama" => Provider::Ollama,
        other => return Err(format!("unknown provider: {other}")),
    };
    let mut headers = Vec::new();
    for entry in value
        .get("headers")
        .and_then(serde_json::Value::as_array)
        .ok_or_else(|| "request is missing headers".to_string())?
    {
        let pair = entry
            .as_array()
            .filter(|pair| pair.len() == 2)
            .ok_or_else(|| "malformed header".to_string())?;
        let name = pair[0]
            .as_str()
            .ok_or_else(|| "malformed header name".to_string())?;
        let value = pair[1]
            .as_str()
            .ok_or_else(|| "malformed header value".to_string())?;
        headers.push((name.to_string(), value.to_string()));
    }
    Ok((
        HttpRequest {
            url: text("url")?,
            headers,
            body: text("body")?,
        },
        provider,
    ))
}

/// Child entry point for [`INTERNAL_MODEL_REQUEST_FLAG`]. Returns the process
/// exit code, or `None` when this invocation is not a model-request child.
///
/// Dispatched before any startup file, history, or session work: this process
/// exists to perform exactly one HTTP request and then die.
pub(crate) fn run_internal_model_request(args: &[std::ffi::OsString]) -> Option<i32> {
    if args.get(1).and_then(|arg| arg.to_str()) != Some(INTERNAL_MODEL_REQUEST_FLAG) {
        return None;
    }
    if args.len() != 2 {
        eprintln!("jsh: internal model request received unexpected arguments");
        return Some(2);
    }
    let answer = |marker: u8, payload: &str| {
        use std::io::Write;
        let mut stdout = std::io::stdout().lock();
        let _ = stdout.write_all(&[marker]);
        let _ = stdout.write_all(payload.as_bytes());
        let _ = stdout.flush();
    };

    let envelope = match crate::io_guard::read_to_end_bounded(
        std::io::stdin().lock(),
        MAX_MODEL_REQUEST_ENVELOPE_BYTES,
    ) {
        Ok(bytes) => bytes,
        Err(error) => {
            answer(MODEL_CHILD_ERR, &format!("could not read request: {error}"));
            return Some(1);
        }
    };
    let envelope = match String::from_utf8(envelope) {
        Ok(text) => text,
        Err(_) => {
            answer(MODEL_CHILD_ERR, "request was not valid UTF-8");
            return Some(1);
        }
    };
    match decode_model_request(&envelope) {
        Ok((request, provider)) => match perform_model_request(request, provider) {
            Ok(reply) => {
                answer(MODEL_CHILD_OK, &reply);
                Some(0)
            }
            Err(error) => {
                answer(MODEL_CHILD_ERR, &error);
                Some(1)
            }
        },
        Err(error) => {
            answer(MODEL_CHILD_ERR, &error);
            Some(1)
        }
    }
}

fn perform_model_request(request: HttpRequest, _provider: Provider) -> Result<String, String> {
    let agent = agent_http_client();
    let mut post = agent.post(&request.url);
    for (name, value) in &request.headers {
        post = post.header(name, value);
    }
    let mut response = post
        .send(request.body.as_str())
        .map_err(|error| error.to_string())?;
    let status = response.status();
    let text = response
        .body_mut()
        .with_config()
        .limit(MAX_AGENT_RESPONSE_BYTES)
        .read_to_string()
        .map_err(|error| format!("read error: {error}"))?;
    if !status.is_success() {
        // The response body is untrusted terminal data. Provider diagnostics
        // are deliberately not echoed before protocol validation.
        return Err(format!("HTTP {}", status.as_u16()));
    }
    // Keep the provider body intact. The parent still owns the
    // PreparedAgentRequest that created this request and parses these bytes
    // through its bound provider/protocol decoder before session ingestion.
    // Parsing in this transport child used to erase completion metadata and
    // made native-tool replies impossible to carry back safely.
    Ok(text)
}

fn agent_http_client() -> ureq::Agent {
    ureq::Agent::config_builder()
        .max_redirects(0)
        .timeout_connect(Some(std::time::Duration::from_secs(5)))
        .timeout_recv_response(Some(std::time::Duration::from_secs(120)))
        .timeout_recv_body(Some(std::time::Duration::from_secs(120)))
        .timeout_send_body(Some(std::time::Duration::from_secs(10)))
        // Keep non-2xx as a normal response so the provider's error body can be
        // read and reported instead of a bare status code.
        .http_status_as_error(false)
        .build()
        .into()
}

fn chat_config(ai_config: &AiConfig) -> ChatConfig {
    // Strict JSON protocol compliance beats creativity here.
    ai_config.chat_config(AGENT_MAX_TOKENS, Some(0.0))
}

fn environment_meta(share_context: bool, cwd: &Path) -> EnvironmentMeta {
    EnvironmentMeta {
        cwd: cwd.display().to_string(),
        shell: "jsh".to_string(),
        os: std::env::consts::OS.to_string(),
        git: if share_context { git_meta(cwd) } else { None },
    }
}

fn git_meta(cwd: &Path) -> Option<GitMeta> {
    let branch = bounded_git_stdout(cwd, &["rev-parse", "--abbrev-ref", "HEAD"], 16 * 1024)
        .and_then(|output| String::from_utf8(output).ok())
        .map(|branch| branch.trim().to_string())
        .filter(|branch| !branch.is_empty())?;
    let dirty = git_worktree_dirty(cwd)?;
    Some(GitMeta {
        branch,
        dirty,
        // Not computed here; None is honest ("unknown/no upstream"), while 0
        // would claim the branch is exactly in sync.
        ahead: None,
        behind: None,
    })
}

fn bounded_git_stdout(cwd: &Path, args: &[&str], max_bytes: usize) -> Option<Vec<u8>> {
    crate::prompt::bounded_git_stdout(cwd, args, max_bytes)
}

/// Keep the Agent probe on the same trusted, process-group-bounded Git funnel
/// as prompt/completion metadata. Oversized repositories simply omit the
/// optional dirty bit.
fn git_worktree_dirty(cwd: &Path) -> Option<bool> {
    bounded_git_stdout(cwd, &["status", "--porcelain"], 128 * 1024).map(|output| !output.is_empty())
}

fn take_agent_interrupt(session: &mut AgentSession, state: &mut ShellState) -> Option<i32> {
    crate::signal::take_pending_status().inspect(|_| {
        session.cancel();
        // SIGINT is consumable so the interactive shell can return to its next
        // prompt. Preserve its control-flow meaning separately: do not run a
        // later command from the same parsed `agent ...; ...` list.
        state.abort_current_program = true;
        eprintln!("agent: interrupted");
    })
}

/// One-shot private state passed to a fresh jsh process for an approved Agent
/// command. Spawning a new process avoids executing Rust after `fork()` in the
/// interactive shell, which always has at least the AI worker thread and may
/// also have a PATH scanner alive.
struct AgentChildTransport {
    dir: PathBuf,
    snapshot: PathBuf,
    claim_dir: PathBuf,
    claimed_snapshot: PathBuf,
    report: PathBuf,
}

impl AgentChildTransport {
    fn new(state: &ShellState, cwd: &Path) -> std::io::Result<Self> {
        let uid = unsafe { nix::libc::geteuid() };
        let timestamp = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|duration| duration.as_nanos())
            .unwrap_or(0);
        let mut last_collision = None;
        for _ in 0..32 {
            let counter = AGENT_CHILD_COUNTER.fetch_add(1, Ordering::Relaxed);
            let dir = std::env::temp_dir().join(format!(
                "jsh-agent-{uid}-{}-{timestamp}-{counter}",
                std::process::id()
            ));
            match fs::DirBuilder::new().mode(0o700).create(&dir) {
                Ok(()) => {
                    let snapshot = dir.join(format!("{AGENT_CHILD_SESSION_ID}.json"));
                    let claim_dir = dir.join(AGENT_CHILD_CLAIM_DIR);
                    let claimed_snapshot = claim_dir.join(format!("{AGENT_CHILD_SESSION_ID}.json"));
                    let report = dir.join("cwd-report");
                    if let Err(error) = fs::DirBuilder::new().mode(0o700).create(&claim_dir) {
                        let _ = fs::remove_dir(&dir);
                        return Err(error);
                    }
                    let mut persisted =
                        crate::session::SessionSnapshot::capture(state, AGENT_CHILD_SESSION_ID);
                    // `SessionSnapshot::apply` restores its cwd before the
                    // child executes anything. Keep that first restore aligned
                    // with the Agent's private cwd too, then set the original
                    // OsStr path exactly in the child for non-UTF-8 paths.
                    persisted.cwd = cwd.to_string_lossy().into_owned();
                    if let Err(error) = persisted.save_to_dir(&dir) {
                        let _ = fs::remove_dir(&claim_dir);
                        let _ = fs::remove_dir(&dir);
                        return Err(error);
                    }
                    return Ok(Self {
                        dir,
                        snapshot,
                        claim_dir,
                        claimed_snapshot,
                        report,
                    });
                }
                Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => {
                    last_collision = Some(error);
                }
                Err(error) => return Err(error),
            }
        }
        Err(last_collision.unwrap_or_else(|| {
            std::io::Error::new(
                std::io::ErrorKind::AlreadyExists,
                "could not allocate private Agent child directory",
            )
        }))
    }
}

impl Drop for AgentChildTransport {
    fn drop(&mut self) {
        // Delete only the exact files jsh created. If an approved command or a
        // same-user process added anything else, leave the private directory
        // behind instead of recursively deleting an unexpected path.
        let _ = fs::remove_file(&self.snapshot);
        let _ = fs::remove_file(&self.claimed_snapshot);
        let _ = fs::remove_file(&self.report);
        let _ = fs::remove_dir(&self.claim_dir);
        let _ = fs::remove_dir(&self.dir);
    }
}

/// Dispatch the undocumented one-shot child mode before normal CLI parsing.
/// The marker alone is insufficient: all three private transport values must
/// be present and the snapshot loader enforces ownership/link/size rules.
pub(crate) fn internal_child_entrypoint() -> Option<i32> {
    let args = std::env::args_os().collect::<Vec<_>>();
    // The transport child is dispatched from the same place, and before the
    // Agent child, so that neither mode can reach startup-file, history, or
    // session work. Both exist to do one bounded thing and exit.
    if let Some(status) = run_internal_model_request(&args) {
        return Some(status);
    }
    if args.get(1).and_then(|arg| arg.to_str()) != Some(INTERNAL_AGENT_CHILD_FLAG) {
        return None;
    }
    let Some(command) = args.get(2).and_then(|arg| arg.to_str()) else {
        eprintln!("jsh: internal Agent child requires one UTF-8 command");
        return Some(2);
    };
    if args.len() != 3 {
        eprintln!("jsh: internal Agent child received unexpected arguments");
        return Some(2);
    }
    Some(run_internal_agent_child(command))
}

fn run_internal_agent_child(command: &str) -> i32 {
    use std::os::unix::ffi::OsStrExt;

    let Some(state_dir) = std::env::var_os(AGENT_CHILD_STATE_DIR_ENV).map(PathBuf::from) else {
        eprintln!("jsh: internal Agent child is missing its state directory");
        return 2;
    };
    let Some(agent_cwd) = std::env::var_os(AGENT_CHILD_CWD_ENV).map(PathBuf::from) else {
        eprintln!("jsh: internal Agent child is missing its working directory");
        return 2;
    };
    let Some(report_path) = std::env::var_os(AGENT_CHILD_REPORT_ENV).map(PathBuf::from) else {
        eprintln!("jsh: internal Agent child is missing its cwd report path");
        return 2;
    };
    for name in [
        AGENT_CHILD_STATE_DIR_ENV,
        AGENT_CHILD_CWD_ENV,
        AGENT_CHILD_REPORT_ENV,
        AGENT_CHILD_COMMAND_ENV,
    ] {
        std::env::remove_var(name);
    }

    let snapshot_path = state_dir.join(format!("{AGENT_CHILD_SESSION_ID}.json"));
    let claim_dir = state_dir.join(AGENT_CHILD_CLAIM_DIR);
    let claimed_snapshot = claim_dir.join(format!("{AGENT_CHILD_SESSION_ID}.json"));
    // `rename` within this private directory is the one-shot claim. Two child
    // processes may be launched with the same capability, but only one can
    // move the source name and therefore reach command execution.
    if let Err(error) = fs::rename(&snapshot_path, &claimed_snapshot) {
        eprintln!("jsh: internal Agent state claim failed: {error}");
        return 1;
    }
    let snapshot_result =
        crate::session::SessionSnapshot::load_from_dir(AGENT_CHILD_SESSION_ID, &claim_dir);
    // The command must not be able to discover or read the serialized shell
    // state through its inherited environment or either known snapshot name.
    let _ = fs::remove_file(&claimed_snapshot);
    let snapshot = match snapshot_result {
        Ok(snapshot) => snapshot,
        Err(error) => {
            eprintln!("jsh: internal Agent state load failed: {error}");
            return 1;
        }
    };

    crate::builtins::reset_exit_request();
    crate::signal::reset_pending_signals();
    crate::signal::install_noninteractive_signals();
    let mut state = ShellState::new(false);
    snapshot.apply(&mut state);
    for name in [
        AGENT_CHILD_STATE_DIR_ENV,
        AGENT_CHILD_CWD_ENV,
        AGENT_CHILD_REPORT_ENV,
        AGENT_CHILD_COMMAND_ENV,
    ] {
        state.unset_var(name);
        std::env::remove_var(name);
    }
    if let Err(error) = std::env::set_current_dir(&agent_cwd) {
        eprintln!("jsh: Agent cwd {agent_cwd:?} is unavailable: {error}");
        return 1;
    }
    state.export_var("PWD", &agent_cwd.to_string_lossy());

    let code = match crate::parser::parse(command) {
        Ok(commands) => crate::executor::execute_program(&commands, &mut state),
        Err(error) => {
            eprintln!("jsh: parse error: {error}");
            2
        }
    };

    if let Ok(final_cwd) = std::env::current_dir() {
        let result = (|| {
            let mut report = OpenOptions::new()
                .write(true)
                .create_new(true)
                .custom_flags(nix::libc::O_NOFOLLOW | nix::libc::O_CLOEXEC)
                .mode(0o600)
                .open(&report_path)?;
            report.set_permissions(fs::Permissions::from_mode(0o600))?;
            report.write_all(final_cwd.as_os_str().as_bytes())
        })();
        if let Err(error) = result {
            eprintln!("jsh: Agent cwd report failed for {report_path:?}: {error}");
        }
    }
    code
}

fn agent_child_command(
    command: &str,
    transport: &AgentChildTransport,
    cwd: &Path,
) -> std::io::Result<std::process::Command> {
    let mut child = std::process::Command::new(std::env::current_exe()?);
    #[cfg(not(test))]
    child.args([INTERNAL_AGENT_CHILD_FLAG, command]);
    #[cfg(test)]
    child
        .args([
            "agent::tests::internal_agent_child_process",
            "--exact",
            "--ignored",
            "--test-threads=1",
        ])
        .env(AGENT_CHILD_COMMAND_ENV, command);
    child
        .env(AGENT_CHILD_STATE_DIR_ENV, &transport.dir)
        .env(AGENT_CHILD_CWD_ENV, cwd)
        .env(AGENT_CHILD_REPORT_ENV, &transport.report);
    Ok(child)
}

fn read_agent_cwd_report(path: &Path) -> std::io::Result<Option<PathBuf>> {
    use std::os::unix::ffi::OsStringExt;
    use std::os::unix::fs::MetadataExt;

    let mut file = match OpenOptions::new()
        .read(true)
        .custom_flags(nix::libc::O_NOFOLLOW | nix::libc::O_NONBLOCK | nix::libc::O_CLOEXEC)
        .open(path)
    {
        Ok(file) => file,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(error) => return Err(error),
    };
    let metadata = file.metadata()?;
    if !metadata.is_file()
        || metadata.nlink() != 1
        || metadata.uid() != unsafe { nix::libc::geteuid() }
        || metadata.len() > 64 * 1024
    {
        return Err(std::io::Error::new(
            std::io::ErrorKind::InvalidData,
            "invalid Agent cwd report",
        ));
    }
    let mut bytes = Vec::new();
    std::io::Read::take(&mut file, 64 * 1024 + 1).read_to_end(&mut bytes)?;
    if bytes.is_empty() || bytes.len() > 64 * 1024 {
        return Err(std::io::Error::new(
            std::io::ErrorKind::InvalidData,
            "invalid Agent cwd report size",
        ));
    }
    Ok(Some(PathBuf::from(std::ffi::OsString::from_vec(bytes))))
}

/// Run one approved command through a fresh jsh process, teeing combined
/// stdout+stderr to the terminal while capturing a bounded observation.
/// Interactive/TTY-dependent programs see a pipe; the Agent protocol already
/// biases toward non-interactive commands. A private one-shot snapshot gives
/// the child the current aliases/functions/options without running Rust code
/// after `fork()` in the multi-threaded interactive process.
fn run_captured(command: &str, state: &mut ShellState, agent_cwd: &mut PathBuf) -> (i32, String) {
    let mut stdout = std::io::stdout();
    run_captured_to(command, state, agent_cwd, &mut stdout)
}

fn run_captured_to(
    command: &str,
    state: &mut ShellState,
    agent_cwd: &mut PathBuf,
    terminal_output: &mut dyn Write,
) -> (i32, String) {
    use nix::unistd::{close, read};
    use std::os::unix::io::{BorrowedFd, IntoRawFd};
    use std::os::unix::process::ExitStatusExt;
    use std::process::Stdio;

    let shell_cwd = std::env::current_dir().unwrap_or_default();
    if *agent_cwd == shell_cwd {
        println!(
            "  {}",
            dim(&format!(
                "$ {}",
                terminal_safe_inline_text(command, MAX_AGENT_COMMAND_DISPLAY_BYTES)
            ))
        );
    } else {
        println!(
            "  {}",
            dim(&format!(
                "$ {}   (in {})",
                terminal_safe_inline_text(command, MAX_AGENT_COMMAND_DISPLAY_BYTES),
                terminal_safe_inline_text(
                    &agent_cwd.display().to_string(),
                    MAX_AGENT_DISPLAY_BYTES
                )
            ))
        );
    }
    let transport = match AgentChildTransport::new(state, agent_cwd) {
        Ok(transport) => transport,
        Err(error) => return (1, format!("[jsh: Agent state snapshot failed: {error}]")),
    };
    // Neither end may leak through the exec that starts the one-shot jsh.
    // `stdout_file` is duplicated onto fd 1 by `Command`; dup2 clears
    // FD_CLOEXEC on that destination, while the original writer and, most
    // importantly, the reader disappear at exec. Without this, a background
    // command inherits both ends of its own capture pipe. Closing `r` here
    // then cannot deliver SIGPIPE to a continuously writing descendant, which
    // leaves the descendant and its waiting jsh parent orphaned indefinitely.
    let (r, w) = match capture_pipe_cloexec() {
        Ok(fds) => (fds.0.into_raw_fd(), fds.1),
        Err(error) => return (1, format!("[jsh: pipe failed: {error}]")),
    };
    let stdout_file = File::from(w);
    let stderr_file = match stdout_file.try_clone() {
        Ok(file) => file,
        Err(error) => {
            close(r).ok();
            return (1, format!("[jsh: pipe clone failed: {error}]"));
        }
    };
    let mut child_command = match agent_child_command(command, &transport, agent_cwd) {
        Ok(command) => command,
        Err(error) => {
            close(r).ok();
            return (1, format!("[jsh: Agent child setup failed: {error}]"));
        }
    };
    child_command
        .stdin(Stdio::inherit())
        .stdout(Stdio::from(stdout_file))
        .stderr(Stdio::from(stderr_file));
    let mut child = match child_command.spawn() {
        Ok(child) => child,
        Err(error) => {
            close(r).ok();
            return (1, format!("[jsh: Agent child spawn failed: {error}]"));
        }
    };
    let child_pid = i32::try_from(child.id()).ok();
    drop(child_command);

    let flags = unsafe { nix::libc::fcntl(r, nix::libc::F_GETFL) };
    if flags < 0
        || unsafe { nix::libc::fcntl(r, nix::libc::F_SETFL, flags | nix::libc::O_NONBLOCK) } < 0
    {
        let _ = child.kill();
        let _ = child.wait();
        close(r).ok();
        return (1, "[jsh: failed to make capture pipe non-blocking]".into());
    }

    let mut captured: Vec<u8> = Vec::new();
    let mut truncated = false;
    let mut buffer = [0_u8; 4096];
    let mut child_status = None;
    let mut output_open = true;
    let mut post_exit_bytes = 0usize;
    let mut forwarded_signal = None;
    let mut refresh_child_status = |status: &mut Option<i32>| {
        if status.is_some() {
            return;
        }
        match child.try_wait() {
            Ok(Some(exit)) => {
                *status = Some(
                    exit.code()
                        .unwrap_or_else(|| 128 + exit.signal().unwrap_or(1)),
                );
            }
            Ok(None) => {}
            Err(_) => {
                let _ = child.kill();
                let _ = child.wait();
                *status = Some(1);
            }
        }
    };
    'capture: loop {
        if let Some(status) = crate::signal::pending_status() {
            let signal = status.saturating_sub(128);
            if signal > 0 && forwarded_signal != Some(signal) {
                // The interactive shell received the terminal signal. Relay
                // it to the one-shot jsh child, whose noninteractive handler
                // forwards it again to any foreground command process group.
                if let Some(child_pid) = child_pid {
                    unsafe {
                        nix::libc::kill(child_pid, signal);
                    }
                }
                forwarded_signal = Some(signal);
            }
        }
        refresh_child_status(&mut child_status);

        while output_open {
            match unsafe { read(BorrowedFd::borrow_raw(r), &mut buffer) } {
                Ok(0) => {
                    output_open = false;
                    break;
                }
                Ok(count) => {
                    let chunk = &buffer[..count];
                    let _ = terminal_output.write_all(chunk);
                    let _ = terminal_output.flush();
                    if captured.len() < MAX_CAPTURED_OUTPUT_BYTES {
                        let room = MAX_CAPTURED_OUTPUT_BYTES - captured.len();
                        captured.extend_from_slice(&chunk[..count.min(room)]);
                        if room <= count {
                            truncated = true;
                        }
                    } else {
                        truncated = true;
                    }
                    // A continuously writing detached descendant can keep the
                    // pipe readable forever. Check the direct jsh child
                    // between chunks rather than waiting for EAGAIN.
                    refresh_child_status(&mut child_status);
                    if child_status.is_some() {
                        post_exit_bytes = post_exit_bytes.saturating_add(count);
                        if post_exit_bytes >= MAX_POST_EXIT_DRAIN_BYTES {
                            truncated = true;
                            break 'capture;
                        }
                    }
                }
                Err(nix::errno::Errno::EINTR) => continue,
                Err(nix::errno::Errno::EAGAIN) => break,
                Err(_) => {
                    output_open = false;
                    break;
                }
            }
        }

        if child_status.is_some() {
            break;
        }
        if output_open {
            let mut descriptor = nix::libc::pollfd {
                fd: r,
                events: nix::libc::POLLIN | nix::libc::POLLHUP | nix::libc::POLLERR,
                revents: 0,
            };
            unsafe {
                nix::libc::poll(&mut descriptor, 1, 100);
            }
        } else {
            std::thread::sleep(std::time::Duration::from_millis(10));
        }
    }
    close(r).ok();
    let exit_code = child_status.unwrap_or(1);
    if let Ok(Some(reported)) = read_agent_cwd_report(&transport.report) {
        if reported != *agent_cwd && reported.is_dir() {
            println!(
                "  {}",
                dim(&format!(
                    "cwd → {}",
                    terminal_safe_inline_text(
                        &reported.as_os_str().to_string_lossy(),
                        MAX_AGENT_DISPLAY_BYTES
                    )
                ))
            );
            *agent_cwd = reported;
        }
    }
    let mut output = String::from_utf8_lossy(&captured).to_string();
    if truncated {
        output.push_str("\n[jsh: further output not captured]");
    }
    (exit_code, output)
}

fn max_turns() -> u32 {
    let configured = std::env::var("JSH_AGENT_MAX_TURNS").ok();
    configured_max_turns(configured.as_deref())
}

fn configured_max_turns(value: Option<&str>) -> u32 {
    value
        .and_then(|value| value.trim().parse::<u32>().ok())
        .filter(|turns| *turns > 0)
        .unwrap_or(DEFAULT_MAX_TURNS)
        .min(MAX_AGENT_SESSION_TURNS)
}

fn configured_agent_protocol(value: Option<&str>) -> Result<AgentProtocol, &'static str> {
    match value.map(str::trim).filter(|value| !value.is_empty()) {
        None | Some("text") => Ok(AgentProtocol::Text),
        Some("native-tools" | "native_tools" | "tools") => Ok(AgentProtocol::NativeTools),
        Some(_) => Err(
            "JSH_AGENT_PROTOCOL must be 'text' or 'native-tools' (text is the compatible default)",
        ),
    }
}

fn env_truthy(name: &str) -> bool {
    std::env::var(name).is_ok_and(|value| {
        matches!(
            value.trim().to_ascii_lowercase().as_str(),
            "1" | "true" | "yes" | "on"
        )
    })
}

fn read_line(prompt: &str) -> Option<String> {
    print!("{prompt}");
    std::io::stdout().flush().ok();
    loop {
        if crate::signal::pending_status().is_some() {
            return None;
        }
        let mut descriptor = nix::libc::pollfd {
            fd: nix::libc::STDIN_FILENO,
            events: nix::libc::POLLIN | nix::libc::POLLHUP | nix::libc::POLLERR,
            revents: 0,
        };
        let result = unsafe { nix::libc::poll(&mut descriptor, 1, 100) };
        if result > 0 {
            break;
        }
        if result < 0 && std::io::Error::last_os_error().kind() != std::io::ErrorKind::Interrupted {
            return None;
        }
    }
    if crate::signal::pending_status().is_some() {
        return None;
    }
    let mut line = String::new();
    match std::io::stdin().read_line(&mut line) {
        Ok(0) | Err(_) => None,
        Ok(_) => Some(line.trim_end_matches(['\r', '\n']).to_string()),
    }
}

fn confirm(prompt: &str) -> bool {
    matches!(
        read_line(prompt).as_deref().map(str::trim),
        Some("y" | "yes")
    )
}

fn status_line(text: &str) {
    println!("{}", dim(&format!("[agent] {text}")));
}

fn styled(code: &str, text: &str) -> String {
    if std::io::stdout().is_terminal() {
        format!("\x1b[{code}m{text}\x1b[0m")
    } else {
        text.to_string()
    }
}

fn emphasize(text: &str) -> String {
    styled("1", text)
}

fn warn(text: &str) -> String {
    styled("31", text)
}

fn dim(text: &str) -> String {
    styled("2", text)
}

/// Unicode controls that can reorder, conceal, or annotate visible terminal
/// text without using a C0/C1 control byte. These are especially dangerous in
/// an exact-command approval card.
/// Render untrusted model, provider, or filesystem text without letting a
/// control sequence or invisible Unicode formatting instruction reach the
/// user's terminal. The rendered result is byte-bounded on UTF-8 boundaries.
fn render_terminal_safe_text(
    value: &str,
    max_bytes: usize,
    initial_prefix: &str,
    continuation_prefix: Option<&str>,
    preserve_newlines: bool,
) -> String {
    let mut output = initial_prefix
        .get(..max_bytes.min(initial_prefix.len()))
        .unwrap_or("")
        .to_string();
    for ch in value.chars() {
        let rendered = match ch {
            '\n' if continuation_prefix.is_some() => {
                format!("\n{}", continuation_prefix.unwrap_or_default())
            }
            '\n' if preserve_newlines => "\n".to_string(),
            '\n' => "\\n".to_string(),
            '\t' => "\\t".to_string(),
            '\r' => "\\r".to_string(),
            ch if crate::terminal_text::is_terminal_ambiguous(ch) && u32::from(ch) <= 0x7f => {
                format!("\\x{:02x}", u32::from(ch))
            }
            ch if crate::terminal_text::is_terminal_ambiguous(ch) => {
                format!("\\u{{{:x}}}", u32::from(ch))
            }
            ch => ch.to_string(),
        };
        if output.len().saturating_add(rendered.len()) > max_bytes {
            if output.len().saturating_add('…'.len_utf8()) <= max_bytes {
                output.push('…');
            }
            break;
        }
        output.push_str(&rendered);
    }
    output
}

fn terminal_safe_text(value: &str, max_bytes: usize) -> String {
    render_terminal_safe_text(value, max_bytes, "", None, true)
}

fn terminal_safe_inline_text(value: &str, max_bytes: usize) -> String {
    render_terminal_safe_text(value, max_bytes, "", None, false)
}

fn terminal_safe_message(prefix: &str, value: &str, max_bytes: usize) -> String {
    render_terminal_safe_text(value, max_bytes, prefix, Some(prefix), true)
}

#[cfg(test)]
mod tests {
    use super::{
        agent_child_command, agent_http_client, bounded_git_stdout, capture_pipe_cloexec,
        configured_agent_protocol, configured_max_turns, git_meta, git_worktree_dirty,
        run_captured, run_captured_to, run_internal_agent_child, run_internal_model_request,
        terminal_safe_inline_text, terminal_safe_message, terminal_safe_text, AgentChildTransport,
        AGENT_CHILD_COMMAND_ENV, MAX_AGENT_COMMAND_DISPLAY_BYTES, MAX_AGENT_DISPLAY_BYTES,
        MAX_AGENT_SESSION_TURNS,
    };
    use super::{decode_model_request, encode_model_request};
    use crate::environment::ShellState;
    use jagent::provider::{ChatConfig, HttpRequest, Message, Provider, Role};
    use jagent::{
        prepare_agent_request as prepare_request, AgentProtocol, AgentRequestSpec as RequestSpec,
        AgentSession, ModelOutcome,
    };
    use std::path::PathBuf;
    use std::time::{Duration, Instant};

    #[test]
    fn a_model_request_envelope_round_trips() {
        let request = HttpRequest {
            url: "https://api.example.test/v1/messages".to_string(),
            headers: vec![
                ("content-type".to_string(), "application/json".to_string()),
                ("x-api-key".to_string(), "sk-secret".to_string()),
            ],
            body: r#"{"model":"m","messages":[]}"#.to_string(),
        };
        for provider in [
            Provider::Anthropic,
            Provider::OpenAiCompatible,
            Provider::Ollama,
        ] {
            let encoded = encode_model_request(&request, provider);
            let (decoded, decoded_provider) = decode_model_request(&encoded).expect("round trip");
            assert_eq!(decoded, request);
            assert_eq!(decoded_provider, provider);
        }
    }

    #[test]
    fn a_malformed_envelope_is_refused_rather_than_guessed() {
        // The envelope carries a URL and an API key into a request this process
        // is about to make. Every field is required; none is defaulted.
        for envelope in [
            "",
            "not json",
            r#"{"v":2,"provider":"ollama","url":"http://x","headers":[],"body":"{}"}"#,
            r#"{"v":1,"provider":"unknown","url":"http://x","headers":[],"body":"{}"}"#,
            r#"{"v":1,"provider":"ollama","headers":[],"body":"{}"}"#,
            r#"{"v":1,"provider":"ollama","url":"http://x","body":"{}"}"#,
            r#"{"v":1,"provider":"ollama","url":"http://x","headers":[],"body":1}"#,
            r#"{"v":1,"provider":"ollama","url":"http://x","headers":[["only-one"]],"body":"{}"}"#,
            r#"{"v":1,"provider":"ollama","url":"http://x","headers":[[1,2]],"body":"{}"}"#,
        ] {
            assert!(
                decode_model_request(envelope).is_err(),
                "accepted {envelope:?}"
            );
        }
    }

    #[test]
    fn the_transport_child_only_answers_to_its_own_flag() {
        let args = |values: &[&str]| -> Vec<std::ffi::OsString> {
            values.iter().map(std::ffi::OsString::from).collect()
        };
        assert_eq!(run_internal_model_request(&args(&["jsh"])), None);
        assert_eq!(
            run_internal_model_request(&args(&["jsh", "-c", "echo"])),
            None
        );
        assert_eq!(
            run_internal_model_request(&args(&["jsh", "--jsh-internal-agent-child", "x"])),
            None
        );
        // Right flag, extra operands: refused rather than ignored, so a stray
        // argument can never be read as part of the request.
        assert_eq!(
            run_internal_model_request(&args(&["jsh", "--jsh-internal-model-request", "extra"])),
            Some(2)
        );
    }

    #[test]
    fn agent_provider_credentials_never_cross_redirects() {
        assert_eq!(agent_http_client().config().max_redirects(), 0);
    }

    #[test]
    #[ignore = "one-shot helper process for Agent command capture tests"]
    fn internal_agent_child_process() {
        use std::io::Write as _;

        let command = std::env::var(AGENT_CHILD_COMMAND_ENV).expect("child command");
        let status = run_internal_agent_child(&command);
        let _ = std::io::stdout().flush();
        let _ = std::io::stderr().flush();
        std::process::exit(status);
    }

    #[test]
    fn one_shot_snapshot_is_atomically_claimed_by_only_one_process() {
        use std::process::Stdio;

        let temp = tempfile::tempdir().expect("tempdir");
        let marker = temp.path().join("executions");
        let state = ShellState::new(false);
        let cwd = std::env::current_dir().unwrap_or_else(|_| PathBuf::from("/"));
        let transport = AgentChildTransport::new(&state, &cwd).expect("Agent transport");
        // Use an external writer so Rust's test-harness output capture cannot
        // intercept the builtin's stdout before the shell redirection.
        let command = format!("/usr/bin/printf x >> '{}'", marker.display());

        let spawn = || {
            let mut command =
                agent_child_command(&command, &transport, &cwd).expect("child command");
            command
                .stdin(Stdio::null())
                .stdout(Stdio::null())
                .stderr(Stdio::null())
                .spawn()
                .expect("spawn Agent child")
        };
        let mut first = spawn();
        let mut second = spawn();
        let mut statuses = [
            first.wait().expect("first Agent child").code(),
            second.wait().expect("second Agent child").code(),
        ];
        statuses.sort_unstable();

        assert_eq!(statuses, [Some(0), Some(1)]);
        assert_eq!(
            std::fs::read_to_string(marker).expect("execution marker"),
            "x"
        );
    }

    #[test]
    fn detached_descendant_cannot_hold_the_capture_loop_open() {
        let mut state = ShellState::new(false);
        let mut cwd = std::env::current_dir().unwrap_or_else(|_| PathBuf::from("/"));
        let started = Instant::now();

        let (status, _) = run_captured("sleep 1 &", &mut state, &mut cwd);

        assert_eq!(status, 0);
        assert!(
            started.elapsed() < Duration::from_millis(750),
            "background stdout descriptor delayed Agent completion for {:?}",
            started.elapsed()
        );
    }

    #[test]
    fn continuously_writing_detached_descendant_cannot_pin_capture() {
        let mut state = ShellState::new(false);
        let mut cwd = std::env::current_dir().unwrap_or_else(|_| PathBuf::from("/"));
        let mut terminal_output = std::io::sink();
        let started = Instant::now();
        let temp = tempfile::tempdir().expect("pid directory");
        let pid_file = temp.path().join("background.pid");
        let quoted_pid_file = pid_file.display().to_string().replace('\'', "'\\''");
        let command = format!(
            "/bin/sh -c 'printf %s \"$$\" > \"$1\"; exec yes agent-background-output' \
             agent-writer '{quoted_pid_file}' & sleep 0.1"
        );

        let (status, observation) =
            run_captured_to(&command, &mut state, &mut cwd, &mut terminal_output);

        assert_eq!(status, 0);
        assert!(
            started.elapsed() < Duration::from_secs(3),
            "continuous background output delayed Agent completion for {:?}",
            started.elapsed()
        );
        assert!(observation.contains("further output not captured"));

        // The capture reader is the only thing keeping the writer alive. Once
        // run_captured_to closes it, the writer must receive SIGPIPE and the
        // background shell waiting for it must disappear as well. The old
        // plain `pipe()` leaked the read end through exec, so this exact test
        // returned quickly while leaving both processes behind forever.
        let background_pid: i32 = std::fs::read_to_string(&pid_file)
            .expect("background pid report")
            .parse()
            .expect("numeric background pid");
        assert!(background_pid > 1, "refuse to probe an unsafe pid");
        let deadline = Instant::now() + Duration::from_secs(2);
        let mut alive = true;
        while Instant::now() < deadline {
            alive = unsafe { nix::libc::kill(background_pid, 0) } == 0;
            if !alive {
                break;
            }
            std::thread::sleep(Duration::from_millis(20));
        }
        if alive {
            // Keep a failed regression test from recreating the leak it is
            // meant to detect. The PID came from this test's own child.
            unsafe {
                nix::libc::kill(background_pid, nix::libc::SIGKILL);
            }
        }
        assert!(!alive, "background Agent writer survived capture teardown");
    }

    #[test]
    fn capture_pipe_marks_both_original_descriptors_close_on_exec() {
        use nix::fcntl::{fcntl, FcntlArg, FdFlag};

        let (reader, writer) = capture_pipe_cloexec().expect("capture pipe");
        for descriptor in [&reader, &writer] {
            let flags = fcntl(descriptor, FcntlArg::F_GETFD).expect("descriptor flags");
            assert!(
                FdFlag::from_bits_truncate(flags).contains(FdFlag::FD_CLOEXEC),
                "capture descriptor must not survive exec"
            );
        }
    }

    #[test]
    fn cwd_report_preserves_significant_trailing_whitespace() {
        let temp = tempfile::tempdir().expect("tempdir");
        let target = temp.path().join("directory with trailing space ");
        std::fs::create_dir(&target).expect("target directory");
        let mut state = ShellState::new(false);
        let mut cwd = std::env::current_dir().unwrap_or_else(|_| PathBuf::from("/"));
        let mut terminal_output = std::io::sink();
        let command = format!("cd '{}'", target.display());

        let (status, _) = run_captured_to(&command, &mut state, &mut cwd, &mut terminal_output);

        assert_eq!(status, 0);
        assert_eq!(cwd, target);
    }

    #[test]
    fn approved_child_inherits_shell_state_without_mutating_parent() {
        let mut state = ShellState::new(false);
        state
            .aliases
            .insert("agent_alias".into(), "/usr/bin/printf alias-ok".into());
        state.set_var("AGENT_SNAPSHOT_VALUE", "parent-value");
        state.shell_opts.nounset = true;
        let definitions = crate::parser::parse("agent_fn() { /usr/bin/printf ':function-ok'; }")
            .expect("function parse");
        assert_eq!(
            crate::executor::execute_program(&definitions, &mut state),
            0
        );

        let mut cwd = std::env::current_dir().unwrap_or_else(|_| PathBuf::from("/"));
        let mut terminal_output = std::io::sink();
        let command = "agent_alias; agent_fn; /usr/bin/printf ':%s:%s' \"$AGENT_SNAPSHOT_VALUE\" \"$-\"; AGENT_SNAPSHOT_VALUE=child-value; alias agent_alias='/usr/bin/printf child-alias'";

        let (status, observation) =
            run_captured_to(command, &mut state, &mut cwd, &mut terminal_output);

        assert_eq!(status, 0);
        assert!(
            observation.contains("alias-ok:function-ok:parent-value:"),
            "restored Agent state produced unexpected output: {observation:?}"
        );
        assert!(
            observation
                .split("alias-ok:function-ok:parent-value:")
                .nth(1)
                .is_some_and(|flags| flags.contains('u')),
            "restored option flags missing nounset: {observation:?}"
        );
        assert_eq!(state.get_var("AGENT_SNAPSHOT_VALUE"), Some("parent-value"));
        assert_eq!(
            state.aliases.get("agent_alias").map(String::as_str),
            Some("/usr/bin/printf alias-ok")
        );
        assert!(state.functions.contains_key("agent_fn"));
    }

    #[test]
    fn git_dirty_probe_reports_clean_and_untracked_worktrees() {
        let temp = tempfile::tempdir().expect("tempdir");
        let git = |args: &[&str]| {
            let status = std::process::Command::new("git")
                .args(args)
                .current_dir(temp.path())
                .status()
                .expect("run git fixture command");
            assert!(status.success(), "git fixture command failed: {args:?}");
        };
        git(&["init", "-q"]);
        std::fs::write(temp.path().join("tracked"), "initial\n").expect("tracked fixture");
        git(&["add", "tracked"]);
        git(&[
            "-c",
            "user.name=jsh test",
            "-c",
            "user.email=jsh@example.invalid",
            "commit",
            "-qm",
            "initial",
        ]);

        let branch = bounded_git_stdout(
            temp.path(),
            &["rev-parse", "--abbrev-ref", "HEAD"],
            16 * 1024,
        )
        .expect("bounded branch probe");
        assert!(!branch.is_empty());
        assert!(
            bounded_git_stdout(temp.path(), &["rev-parse", "--abbrev-ref", "HEAD"], 1).is_none(),
            "oversized Git probe output must fail closed"
        );
        assert_eq!(git_worktree_dirty(temp.path()), Some(false));
        assert_eq!(git_meta(temp.path()).map(|meta| meta.dirty), Some(false));
        std::fs::write(temp.path().join("untracked"), "new\n").expect("untracked fixture");
        assert_eq!(git_worktree_dirty(temp.path()), Some(true));
        assert_eq!(git_meta(temp.path()).map(|meta| meta.dirty), Some(true));
    }

    #[test]
    fn untrusted_agent_text_cannot_emit_terminal_controls_or_grow_unbounded() {
        let hostile = format!(
            "before\x1b]52;c;Y2xpcGJvYXJk\x07after\rnext\u{0085}\u{202e}\u{200b}{}",
            "界".repeat(MAX_AGENT_DISPLAY_BYTES)
        );

        let rendered = terminal_safe_text(&hostile, MAX_AGENT_DISPLAY_BYTES);

        assert!(rendered.len() <= MAX_AGENT_DISPLAY_BYTES);
        assert!(!rendered.contains('\x1b'));
        assert!(!rendered.contains('\x07'));
        assert!(!rendered.contains('\r'));
        assert!(!rendered.contains('\u{0085}'));
        assert!(rendered.contains("\\x1b]52"));
        assert!(rendered.contains("\\r"));
        assert!(rendered.contains("\\u{85}"));
        assert!(rendered.contains("\\u{202e}"));
        assert!(rendered.contains("\\u{200b}"));
        assert!(rendered.is_char_boundary(rendered.len()));

        let inline = terminal_safe_inline_text("first\nsecond", MAX_AGENT_DISPLAY_BYTES);
        assert_eq!(inline, "first\\nsecond");
        let message = terminal_safe_message("agent: ", "first\nsecond", MAX_AGENT_DISPLAY_BYTES);
        assert_eq!(message, "agent: first\nagent: second");

        // The maximum-size command remains fully reviewable even when every
        // character expands into an explicit Unicode escape.
        let ambiguous = "\u{00ad}".repeat((16 * 1024) / '\u{00ad}'.len_utf8());
        let command_display =
            terminal_safe_inline_text(&ambiguous, MAX_AGENT_COMMAND_DISPLAY_BYTES);
        assert!(!command_display.ends_with('…'));
        assert_eq!(
            command_display.matches("\\u{ad}").count(),
            ambiguous.chars().count()
        );
    }

    #[test]
    fn jagent_boundaries_and_protocol_selection_are_shared() {
        assert_eq!(configured_max_turns(None), 16);
        assert_eq!(configured_max_turns(Some("0")), 16);
        assert_eq!(
            configured_max_turns(Some("4294967295")),
            MAX_AGENT_SESSION_TURNS
        );
        assert_eq!(
            configured_agent_protocol(None),
            Ok(jagent::AgentProtocol::Text)
        );
        assert_eq!(
            configured_agent_protocol(Some("native-tools")),
            Ok(jagent::AgentProtocol::NativeTools)
        );
        assert!(configured_agent_protocol(Some("guess")).is_err());
        for command in [
            "hostname build-node",
            "date --set=tomorrow",
            "truncate -s 0 database",
            "git restore src/main.rs",
            "git checkout -- src/main.rs",
            "git branch -D work",
            "git stash clear",
            "docker volume rm database",
            "kubectl delete namespace prod",
            "terraform destroy -auto-approve",
        ] {
            assert!(jagent::is_dangerous(command).is_some(), "missed {command}");
        }
    }

    #[test]
    fn native_tools_provider_envelopes_follow_the_prepared_request_into_session() {
        let fixtures: [(Provider, &[u8]); 3] = [
            (
                Provider::Anthropic,
                br#"{"content":[{"type":"tool_use","id":"toolu_1","name":"run","input":{"command":"pwd"}}],"stop_reason":"tool_use"}"#,
            ),
            (
                Provider::OpenAiCompatible,
                br#"{"choices":[{"message":{"content":null,"tool_calls":[{"id":"call_1","type":"function","function":{"name":"run","arguments":"{\"command\":\"pwd\"}"}}]},"finish_reason":"tool_calls"}]}"#,
            ),
            (
                Provider::Ollama,
                br#"{"message":{"content":"","tool_calls":[{"function":{"name":"run","arguments":{"command":"pwd"}}}]},"done":true,"done_reason":"stop"}"#,
            ),
        ];

        for (provider, body) in fixtures {
            let history = [Message {
                role: Role::User,
                text: "inspect".into(),
            }];
            let config = ChatConfig {
                provider,
                api_key: (provider == Provider::Anthropic).then(|| "test-key".into()),
                model: "test-model".into(),
                base_url: provider.default_base_url().into(),
                max_tokens: 128,
                temperature: Some(0.0),
            };
            let prepared = prepare_request(
                &config,
                RequestSpec::new(&history, AgentProtocol::NativeTools),
            )
            .unwrap();
            assert_eq!(prepared.protocol(), AgentProtocol::NativeTools);
            assert!(prepared.request.body.contains("\"tools\""));

            let response = prepared.parse_response(body).unwrap();
            let mut session = AgentSession::new(4);
            session.submit_user("inspect").unwrap();
            let ModelOutcome::Proposal { command, .. } =
                session.accept_agent_response(&response).unwrap()
            else {
                panic!("{provider:?} did not produce a proposal")
            };
            assert_eq!(command, "pwd", "{provider:?}");
        }
    }
}
