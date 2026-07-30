//! Interactive review-first AI agent built on the shared `jagent` core.
//!
//! The model may only propose commands; every proposal goes through an
//! explicit approval prompt (or the opt-in read-only auto-approval allowlist)
//! before jsh executes it. Approved commands run in a forked child through the
//! normal jsh parser/executor with stdout+stderr teed to the terminal and
//! captured as the bounded observation for the next model turn.
//!
//! Configuration reuses the `JSH_AI_*` environment contract from `crate::ai`,
//! plus:
//! - `JSH_AGENT_MAX_TURNS` — model-turn budget (default 16)
//! - `JSH_AGENT_AUTO_APPROVE_READONLY` — auto-run commands accepted by
//!   `jagent::is_auto_approvable` (fail-closed allowlist; everything else
//!   still prompts)

use crate::ai::{AiConfig, AiProvider};
use crate::environment::ShellState;
use jagent::provider::{build_chat_request, parse_chat_response, ChatConfig, Message};
use jagent::{
    AgentSession, AgentState, ApprovedCommand, EnvironmentMeta, GitMeta, ModelOutcome, Provider,
    Role, SessionError,
};
use std::io::{IsTerminal, Write};
use std::path::{Path, PathBuf};

const DEFAULT_MAX_TURNS: u32 = 16;
const AGENT_MAX_TOKENS: u32 = 1024;
/// Collect at most this much execution output; `AgentSession::observe` samples
/// it further down to its own observation budget.
const MAX_CAPTURED_OUTPUT_BYTES: usize = 128 * 1024;
const MAX_CONSECUTIVE_PROTOCOL_RETRIES: u32 = 2;

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

    let auto_readonly = env_truthy("JSH_AGENT_AUTO_APPROVE_READONLY");
    let mut protocol_retries = 0_u32;
    // The agent's working directory persists across its own turns (each
    // approved command starts here, and a command's `cd` carries forward)
    // without ever changing the interactive shell's cwd.
    let mut agent_cwd = std::env::current_dir().unwrap_or_else(|_| PathBuf::from("/"));

    loop {
        match session.state() {
            AgentState::AwaitingModel => {
                status_line(&format!(
                    "thinking… (turn {}/{})",
                    session.turns_used() + 1,
                    session.max_turns()
                ));
                let reply = match request_model(&chat, &session, share_context, &agent_cwd) {
                    Ok(reply) => reply,
                    Err(error) => {
                        let _ = session.model_failed(&error);
                        eprintln!("agent: model request failed: {error}");
                        if session.can_retry_model() && confirm("retry? [y/N] ") {
                            let _ = session.retry_model();
                            continue;
                        }
                        return 1;
                    }
                };
                match session.accept_model_reply(&reply) {
                    Ok(ModelOutcome::Proposal {
                        id,
                        command,
                        danger,
                    }) => {
                        protocol_retries = 0;
                        let approved = match review_proposal(
                            &mut session,
                            id,
                            &command,
                            danger,
                            auto_readonly,
                        ) {
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
                        if let Err(error) =
                            session.observe(approved.proposal_id, exit_code, &output)
                        {
                            eprintln!("agent: {error}");
                            return 1;
                        }
                    }
                    Ok(ModelOutcome::Said(message)) => {
                        protocol_retries = 0;
                        println!("agent: {message}");
                    }
                    Ok(ModelOutcome::Completed(message)) => {
                        protocol_retries = 0;
                        println!("agent done: {message}");
                    }
                    Err(SessionError::Protocol(error)) => {
                        eprintln!("agent: model reply violated the protocol: {error}");
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
    auto_readonly: bool,
) -> ReviewOutcome {
    println!();
    println!("  proposed: {}", emphasize(command));
    if let Some(reason) = danger {
        println!("  {}", warn(&format!("warning: {reason}")));
    }
    if auto_readonly && danger.is_none() && jagent::is_auto_approvable(command) {
        println!("  auto-approved (read-only allowlist)");
        match session.approve(id) {
            Ok(approved) => return ReviewOutcome::Approved(approved),
            Err(error) => {
                eprintln!("agent: {error}");
                return ReviewOutcome::Quit;
            }
        }
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
                if !confirm_danger(&approved) {
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
                match session.edit_and_approve(id, edited) {
                    Ok(approved) => {
                        if !confirm_danger(&approved) {
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
fn confirm_danger(approved: &ApprovedCommand) -> bool {
    let Some(reason) = approved.danger else {
        return true;
    };
    println!("  {}", warn(&format!("dangerous: {reason}")));
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
) -> Result<String, String> {
    let environment = environment_meta(share_context, agent_cwd);
    // Transcript observations replay real terminal output; scrub
    // high-confidence secret shapes before anything leaves the machine.
    let user_text = jagent::redact_secrets(&jagent::agent_user_prompt(
        &session.build_user_prompt(),
        &environment,
        None,
    ));
    let request = build_chat_request(
        chat,
        Some(&jagent::build_agent_system_prompt()),
        &[Message {
            role: Role::User,
            text: user_text,
        }],
    )
    .map_err(|error| error.to_string())?;

    let agent: ureq::Agent = ureq::Agent::config_builder()
        .timeout_connect(Some(std::time::Duration::from_secs(5)))
        .timeout_recv_response(Some(std::time::Duration::from_secs(120)))
        .timeout_recv_body(Some(std::time::Duration::from_secs(120)))
        .timeout_send_body(Some(std::time::Duration::from_secs(10)))
        // Keep non-2xx as a normal response so the provider's error body can be
        // read and reported instead of a bare status code.
        .http_status_as_error(false)
        .build()
        .into();
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
        .read_to_string()
        .map_err(|error| format!("read error: {error}"))?;
    if !status.is_success() {
        return Err(format!("HTTP {}: {text}", status.as_u16()));
    }
    let json: serde_json::Value =
        serde_json::from_str(&text).map_err(|error| format!("invalid response JSON: {error}"))?;
    parse_chat_response(chat.provider, &json).map_err(|error| error.to_string())
}

fn chat_config(ai_config: &AiConfig) -> ChatConfig {
    let provider = match ai_config.provider {
        AiProvider::OpenAI => Provider::OpenAiCompatible,
        AiProvider::Anthropic => Provider::Anthropic,
        AiProvider::Ollama => Provider::Ollama,
    };
    // jsh's base_url contract matches the provider defaults (no trailing
    // path); jagent's endpoint() appends the per-provider path.
    ChatConfig {
        provider,
        api_key: ai_config.api_key.clone(),
        model: ai_config.model.clone(),
        base_url: ai_config.base_url.clone(),
        max_tokens: AGENT_MAX_TOKENS,
        // Strict JSON protocol compliance beats creativity here.
        temperature: Some(0.0),
    }
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
    let branch = std::process::Command::new("git")
        .args(["rev-parse", "--abbrev-ref", "HEAD"])
        .current_dir(cwd)
        .output()
        .ok()
        .filter(|output| output.status.success())
        .and_then(|output| String::from_utf8(output.stdout).ok())
        .map(|branch| branch.trim().to_string())
        .filter(|branch| !branch.is_empty())?;
    let dirty = std::process::Command::new("git")
        .args(["status", "--porcelain"])
        .current_dir(cwd)
        .output()
        .ok()
        .filter(|output| output.status.success())
        .is_some_and(|output| !output.stdout.is_empty());
    Some(GitMeta {
        branch,
        dirty,
        // Not computed here; None is honest ("unknown/no upstream"), while 0
        // would claim the branch is exactly in sync.
        ahead: None,
        behind: None,
    })
}

/// Run one approved command through the jsh parser/executor in a forked child,
/// teeing combined stdout+stderr to the terminal while capturing a bounded
/// copy for the observation. Interactive/TTY-dependent programs will see a
/// pipe; the agent protocol already biases toward non-interactive commands.
///
/// The command starts in `agent_cwd`, and the child reports its final working
/// directory back over a dedicated pipe so `cd` persists across agent turns
/// while the interactive shell's own cwd stays untouched.
fn run_captured(command: &str, state: &mut ShellState, agent_cwd: &mut PathBuf) -> (i32, String) {
    use nix::sys::wait::{waitpid, WaitStatus};
    use nix::unistd::{close, fork, pipe, read, write, ForkResult};
    use std::os::unix::io::{BorrowedFd, IntoRawFd};

    let shell_cwd = std::env::current_dir().unwrap_or_default();
    if *agent_cwd == shell_cwd {
        println!("  {}", dim(&format!("$ {command}")));
    } else {
        println!(
            "  {}",
            dim(&format!("$ {command}   (in {})", agent_cwd.display()))
        );
    }
    let (r, w) = match pipe() {
        Ok(fds) => (fds.0.into_raw_fd(), fds.1.into_raw_fd()),
        Err(error) => return (1, format!("[jsh: pipe failed: {error}]")),
    };
    let (status_r, status_w) = match pipe() {
        Ok(fds) => (fds.0.into_raw_fd(), fds.1.into_raw_fd()),
        Err(error) => {
            close(r).ok();
            close(w).ok();
            return (1, format!("[jsh: pipe failed: {error}]"));
        }
    };

    match unsafe { fork() } {
        Ok(ForkResult::Child) => {
            close(r).ok();
            close(status_r).ok();
            unsafe {
                nix::libc::dup2(w, 1);
                nix::libc::dup2(w, 2);
                // Executed programs must not inherit the cwd-report pipe.
                nix::libc::fcntl(status_w, nix::libc::F_SETFD, nix::libc::FD_CLOEXEC);
            }
            close(w).ok();
            state.interactive = false;
            if std::env::set_current_dir(&*agent_cwd).is_ok() {
                state.export_var("PWD", &agent_cwd.display().to_string());
            }
            let code = match crate::parser::parse(command) {
                Ok(commands) => {
                    let mut code = 0;
                    for parsed in &commands {
                        code = crate::executor::execute_complete_command(parsed, state);
                    }
                    code
                }
                Err(error) => {
                    eprintln!("jsh: parse error: {error}");
                    2
                }
            };
            if let Ok(final_cwd) = std::env::current_dir() {
                let bytes = final_cwd.display().to_string().into_bytes();
                let _ = write(unsafe { BorrowedFd::borrow_raw(status_w) }, &bytes);
            }
            close(status_w).ok();
            std::process::exit(code);
        }
        Ok(ForkResult::Parent { child }) => {
            close(w).ok();
            close(status_w).ok();
            let mut captured: Vec<u8> = Vec::new();
            let mut truncated = false;
            let mut buffer = [0_u8; 4096];
            let stdout = std::io::stdout();
            loop {
                match unsafe { read(BorrowedFd::borrow_raw(r), &mut buffer) } {
                    Ok(0) | Err(_) => break,
                    Ok(count) => {
                        let chunk = &buffer[..count];
                        let mut out = stdout.lock();
                        let _ = out.write_all(chunk);
                        let _ = out.flush();
                        if captured.len() < MAX_CAPTURED_OUTPUT_BYTES {
                            let room = MAX_CAPTURED_OUTPUT_BYTES - captured.len();
                            captured.extend_from_slice(&chunk[..count.min(room)]);
                            if room <= count {
                                truncated = true;
                            }
                        } else {
                            truncated = true;
                        }
                    }
                }
            }
            close(r).ok();
            let mut reported_cwd: Vec<u8> = Vec::new();
            loop {
                match unsafe { read(BorrowedFd::borrow_raw(status_r), &mut buffer) } {
                    Ok(0) | Err(_) => break,
                    Ok(count) => reported_cwd.extend_from_slice(&buffer[..count]),
                }
            }
            close(status_r).ok();
            let exit_code = match waitpid(child, None) {
                Ok(WaitStatus::Exited(_, code)) => code,
                Ok(WaitStatus::Signaled(_, signal, _)) => 128 + signal as i32,
                _ => 1,
            };
            if let Ok(reported) = String::from_utf8(reported_cwd) {
                let reported = reported.trim();
                if !reported.is_empty() {
                    let reported = PathBuf::from(reported);
                    if reported != *agent_cwd && reported.is_dir() {
                        println!("  {}", dim(&format!("cwd → {}", reported.display())));
                        *agent_cwd = reported;
                    }
                }
            }
            let mut output = String::from_utf8_lossy(&captured).to_string();
            if truncated {
                output.push_str("\n[jsh: further output not captured]");
            }
            (exit_code, output)
        }
        Err(error) => {
            close(r).ok();
            close(w).ok();
            close(status_r).ok();
            close(status_w).ok();
            (1, format!("[jsh: fork failed: {error}]"))
        }
    }
}

fn max_turns() -> u32 {
    std::env::var("JSH_AGENT_MAX_TURNS")
        .ok()
        .and_then(|value| value.trim().parse::<u32>().ok())
        .filter(|turns| *turns > 0)
        .unwrap_or(DEFAULT_MAX_TURNS)
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
