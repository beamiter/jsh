/// Prompt rendering with colors, git branch, exit code.
use crate::environment::{PromptStyle, ShellState};
use crossterm::style::{Color, Stylize};
use std::env;
use std::path::Path;

const GIT_PROBE_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(2);
const MAX_GIT_PROMPT_BYTES: usize = 256 * 1024;

#[derive(Debug, Default, PartialEq, Eq)]
pub struct GitContext {
    pub branch: Option<String>,
    pub remote: Option<String>,
    pub has_staged: bool,
    pub has_unstaged: bool,
    pub has_conflicts: bool,
    pub ahead: usize,
    pub behind: usize,
}

pub fn render_prompt(state: &ShellState) -> String {
    // Determine effective style
    let style = match state.prompt_style {
        PromptStyle::Auto => {
            // Auto-select based on terminal width
            // If terminal < 60 cols, use minimal
            // If terminal < 100 cols, use compact
            // Otherwise use full
            if state.terminal_width < 60 {
                PromptStyle::Minimal
            } else if state.terminal_width < 100 {
                PromptStyle::Compact
            } else {
                PromptStyle::Full
            }
        }
        other => other,
    };

    match style {
        PromptStyle::Full => render_prompt_full(state),
        PromptStyle::Compact => render_prompt_compact(state),
        PromptStyle::Minimal => render_prompt_minimal(state),
        PromptStyle::Auto => unreachable!(), // Already resolved above
    }
}

/// Full prompt: user@host ~/path (branch) took duration ❯
fn render_prompt_full(state: &ShellState) -> String {
    let user = crate::terminal_text::escape_inline(
        &env::var("USER").unwrap_or_else(|_| String::from("user")),
        256,
    );
    let hostname = crate::terminal_text::escape_inline(&state.hostname, 256);
    let cwd = get_short_cwd(state);
    let git_branch = state
        .cached_git_branch
        .as_deref()
        .map(|branch| crate::terminal_text::escape_inline(branch, 1024));
    let env_hint = get_env_hint().map(|hint| crate::terminal_text::escape_inline(&hint, 1024));
    let exit_indicator = if state.last_exit_code == 0 {
        "❯".green().bold().to_string()
    } else {
        "❯".red().bold().to_string()
    };

    let mut prompt = String::new();

    if let Some(hint) = env_hint {
        prompt.push_str(&format!("{} ", hint.with(Color::Yellow).bold()));
    }

    // User@host
    prompt.push_str(&format!(
        "{}",
        format!("{}@{}", user, hostname)
            .with(Color::Rgb {
                r: 0,
                g: 210,
                b: 210
            })
            .bold()
    ));
    prompt.push(' ');

    // CWD
    prompt.push_str(&format!(
        "{}",
        cwd.with(Color::Rgb {
            r: 80,
            g: 255,
            b: 120
        })
        .bold()
    ));

    // Git branch in magenta
    if let Some(branch) = git_branch.as_deref() {
        prompt.push_str(&format!(" {}", format!("({})", branch).magenta()));
    }

    // Command duration
    if let Some(d) = &state.last_command_duration {
        let dur_str = format_duration(*d);
        if !dur_str.is_empty() {
            prompt.push_str(&format!(
                " {}",
                format!("took {}", dur_str).with(Color::Yellow)
            ));
        }
    }

    prompt.push(' ');
    prompt.push_str(&exit_indicator);
    prompt.push(' ');

    prompt
}

/// Compact prompt: user ~/path (branch) ❯
fn render_prompt_compact(state: &ShellState) -> String {
    let user = crate::terminal_text::escape_inline(
        &env::var("USER").unwrap_or_else(|_| String::from("user")),
        256,
    );
    let cwd = get_short_cwd(state);
    let git_branch = state
        .cached_git_branch
        .as_deref()
        .map(|branch| crate::terminal_text::escape_inline(branch, 1024));
    let env_hint = get_env_hint().map(|hint| crate::terminal_text::escape_inline(&hint, 1024));
    let exit_indicator = if state.last_exit_code == 0 {
        "❯".green().bold().to_string()
    } else {
        "❯".red().bold().to_string()
    };

    let mut prompt = String::new();

    if let Some(hint) = env_hint {
        prompt.push_str(&format!("{} ", hint.with(Color::Yellow).bold()));
    }

    // User only (no hostname)
    prompt.push_str(&format!(
        "{}",
        user.with(Color::Rgb {
            r: 0,
            g: 210,
            b: 210
        })
        .bold()
    ));
    prompt.push(' ');

    // CWD
    prompt.push_str(&format!(
        "{}",
        cwd.with(Color::Rgb {
            r: 80,
            g: 255,
            b: 120
        })
        .bold()
    ));

    // Git branch in magenta
    if let Some(branch) = git_branch.as_deref() {
        prompt.push_str(&format!(" {}", format!("({})", branch).magenta()));
    }

    prompt.push(' ');
    prompt.push_str(&exit_indicator);
    prompt.push(' ');

    prompt
}

/// Minimal prompt: ~/path ❯
fn render_prompt_minimal(state: &ShellState) -> String {
    let cwd = get_short_cwd(state);
    let env_hint = get_env_hint().map(|hint| crate::terminal_text::escape_inline(&hint, 1024));
    let exit_indicator = if state.last_exit_code == 0 {
        "❯".green().bold().to_string()
    } else {
        "❯".red().bold().to_string()
    };

    let mut prompt = String::new();

    if let Some(hint) = env_hint {
        prompt.push_str(&format!("{} ", hint.with(Color::Yellow).bold()));
    }

    // CWD only
    prompt.push_str(&format!(
        "{}",
        cwd.with(Color::Rgb {
            r: 80,
            g: 255,
            b: 120
        })
        .bold()
    ));

    prompt.push(' ');
    prompt.push_str(&exit_indicator);
    prompt.push(' ');

    prompt
}

pub fn get_short_cwd(state: &ShellState) -> String {
    let cwd = env::current_dir()
        .map(|p| p.to_string_lossy().to_string())
        .unwrap_or_else(|_| String::from("?"));

    let home = state.home_dir.to_string_lossy();
    let short = if cwd.starts_with(home.as_ref()) {
        format!("~{}", &cwd[home.len()..])
    } else {
        cwd
    };
    crate::terminal_text::escape_inline(&short, 16 * 1024)
}

/// Discover the current branch and its tracking remote with one Git process.
/// `status --porcelain=v2 --branch` works for normal repositories, worktrees,
/// submodules, and repositories whose `.git` directory lives elsewhere.
pub fn probe_git_context() -> GitContext {
    let Some(output) = bounded_git_stdout(
        Path::new("."),
        &["status", "--porcelain=v2", "--branch"],
        MAX_GIT_PROMPT_BYTES,
    ) else {
        return GitContext::default();
    };

    parse_git_status_header(&String::from_utf8_lossy(&output))
}

/// Run a read-only Git probe without letting a large worktree pin the prompt
/// or allocate an unbounded `Command::output` buffer.
pub(crate) fn bounded_git_stdout(cwd: &Path, args: &[&str], max_bytes: usize) -> Option<Vec<u8>> {
    let git = crate::io_guard::trusted_helper("git")?;
    let mut command = std::process::Command::new(git);
    command
        .args(args)
        .current_dir(cwd)
        .env("GIT_OPTIONAL_LOCKS", "0");
    let output = crate::io_guard::bounded_command_output(
        &mut command,
        max_bytes,
        64 * 1024,
        GIT_PROBE_TIMEOUT,
    )
    .ok()?;
    output.status.success().then_some(output.stdout)
}

fn parse_git_status_header(status: &str) -> GitContext {
    let mut context = GitContext::default();
    for line in status.lines() {
        if let Some(branch) = line.strip_prefix("# branch.head ") {
            if branch != "(detached)" {
                context.branch = Some(branch.to_string());
            }
        } else if let Some(upstream) = line.strip_prefix("# branch.upstream ") {
            context.remote = upstream
                .split_once('/')
                .map(|(remote, _)| remote.to_string());
        } else if let Some(ab) = line.strip_prefix("# branch.ab ") {
            for value in ab.split_whitespace() {
                if let Some(ahead) = value.strip_prefix('+') {
                    context.ahead = ahead.parse().unwrap_or(0);
                } else if let Some(behind) = value.strip_prefix('-') {
                    context.behind = behind.parse().unwrap_or(0);
                }
            }
        } else if line.starts_with("? ") {
            context.has_unstaged = true;
        } else if line.starts_with("u ") {
            context.has_conflicts = true;
        } else if line.starts_with("1 ") || line.starts_with("2 ") {
            if let Some(status) = line.split_whitespace().nth(1) {
                let mut chars = status.chars();
                context.has_staged |= !matches!(chars.next(), None | Some('.'));
                context.has_unstaged |= !matches!(chars.next(), None | Some('.'));
            }
        }
    }
    context
}

pub fn get_git_branch() -> Option<String> {
    probe_git_context().branch
}

fn get_env_hint() -> Option<String> {
    if let Ok(modifier) = env::var("CONDA_PROMPT_MODIFIER") {
        let trimmed = modifier.trim();
        if !trimmed.is_empty() {
            return Some(trimmed.to_string());
        }
    }

    if let Ok(venv) = env::var("VIRTUAL_ENV") {
        let trimmed = venv.trim();
        if !trimmed.is_empty() {
            if let Some(name) = std::path::Path::new(trimmed).file_name() {
                let name = name.to_string_lossy().trim().to_string();
                if !name.is_empty() {
                    return Some(format!("({})", name));
                }
            }
        }
    }

    None
}

/// Render the continuation prompt for multiline input.
pub fn render_continuation_prompt() -> String {
    format!("{} ", "> ".dark_grey())
}

/// Render the right prompt (command duration).
pub fn render_rprompt(state: &ShellState) -> String {
    let mut rprompt = String::new();
    if let Some(d) = &state.last_command_duration {
        let dur_str = format_duration(*d);
        if !dur_str.is_empty() {
            rprompt.push_str(&format!("{}", dur_str.with(Color::Yellow)));
        }
    }
    rprompt
}

/// Get the raw (no ANSI) width of the rprompt for positioning.
pub fn rprompt_width(state: &ShellState) -> usize {
    if let Some(d) = &state.last_command_duration {
        let dur_str = format_duration(*d);
        dur_str.len()
    } else {
        0
    }
}

pub fn format_duration(d: std::time::Duration) -> String {
    let secs = d.as_secs();
    if secs >= 3600 {
        format!("{}h{}m{}s", secs / 3600, (secs % 3600) / 60, secs % 60)
    } else if secs >= 60 {
        format!("{}m{}s", secs / 60, secs % 60)
    } else if secs >= 1 {
        format!("{:.1}s", d.as_secs_f64())
    } else {
        String::new() // Don't show for sub-second
    }
}

#[cfg(test)]
mod tests {
    use super::{get_env_hint, parse_git_status_header, GitContext};
    use std::sync::Mutex;

    static ENV_LOCK: Mutex<()> = Mutex::new(());

    #[test]
    fn parses_main_or_master_and_tracking_remote_from_one_status() {
        let main = parse_git_status_header(
            "# branch.oid abc\n# branch.head main\n# branch.upstream origin/main\n# branch.ab +2 -1\n1 M. N... file.rs\n? new.rs\n",
        );
        assert_eq!(
            main,
            GitContext {
                branch: Some("main".into()),
                remote: Some("origin".into()),
                has_staged: true,
                has_unstaged: true,
                ahead: 2,
                behind: 1,
                ..GitContext::default()
            }
        );

        let master = parse_git_status_header("# branch.oid def\n# branch.head master\n");
        assert_eq!(master.branch.as_deref(), Some("master"));
        assert_eq!(master.remote, None);
    }

    #[test]
    fn env_hint_prefers_conda_prompt_modifier() {
        let _guard = ENV_LOCK.lock().unwrap();
        unsafe {
            std::env::set_var("CONDA_PROMPT_MODIFIER", "(GPTSoVits) ");
            std::env::set_var("VIRTUAL_ENV", "/tmp/.venv");
        }
        assert_eq!(get_env_hint().as_deref(), Some("(GPTSoVits)"));
        unsafe {
            std::env::remove_var("CONDA_PROMPT_MODIFIER");
            std::env::remove_var("VIRTUAL_ENV");
        }
    }

    #[test]
    fn env_hint_falls_back_to_virtual_env_name() {
        let _guard = ENV_LOCK.lock().unwrap();
        unsafe {
            std::env::remove_var("CONDA_PROMPT_MODIFIER");
            std::env::set_var("VIRTUAL_ENV", "/tmp/myenv");
        }
        assert_eq!(get_env_hint().as_deref(), Some("(myenv)"));
        unsafe {
            std::env::remove_var("VIRTUAL_ENV");
        }
    }
}
