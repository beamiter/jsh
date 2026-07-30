/// Config file loading: source ~/.bashrc or ~/.jshrc on startup.
use crate::environment::{ConfigSource, ShellState};
use crate::executor;
use crate::parser;
use std::path::Path;
use std::process::Command;

pub fn load_config(state: &mut ShellState) {
    match state.shell_opts.config_source {
        ConfigSource::Bashrc => load_bashrc(state),
        ConfigSource::Jshrc => load_jshrc(state),
    }
}

pub fn refresh_shell_integrations(state: &mut ShellState) {
    load_conda_hook(state);
}

/// Load an explicitly selected startup file.
///
/// Native jsh syntax is attempted first. Files using syntax that jsh cannot
/// parse are imported through the same Bash compatibility bridge as `.bashrc`.
pub fn load_config_file(path: &Path, state: &mut ShellState) {
    source_file_lenient(path, state);
}

/// Load ~/.bashrc directly via bash, without attempting jsh parsing
fn load_bashrc(state: &mut ShellState) {
    let bashrc = state.home_dir.join(".bashrc");
    if bashrc.exists() {
        source_via_bash(&bashrc, state);
    }
}

/// Load ~/.jshrc via jsh parser with bash fallback for complex scripts
fn load_jshrc(state: &mut ShellState) {
    let jshrc = state.home_dir.join(".jshrc");
    if jshrc.exists() {
        source_file_lenient(&jshrc, state);
    }
}

/// Load bash file with lenient error handling - use bash as fallback for complex scripts
fn source_file_lenient(path: &Path, state: &mut ShellState) {
    let Ok(content) = std::fs::read_to_string(path) else {
        return; // Missing default startup files are normal.
    };
    // Try parsing the entire file first.
    match parser::parse(&content) {
        Ok(commands) => {
            // Execute the startup file as one program so exit, failglob, and
            // errexit stop the remaining rc commands.
            executor::execute_program(&commands, state);
        }
        Err(_) => {
            // Full parse failed, use bash as fallback for complex scripts.
            source_via_bash(path, state);
        }
    }
}

/// Use bash to source a script file and extract environment variables, aliases, functions, and options
fn source_via_bash(path: &Path, state: &mut ShellState) {
    // `$1` transports the path as data. Interpolating it into this program would
    // make quotes, command substitutions, or newlines in a filename executable.
    let bash_script = r#"
# Set PS1 to make bash think it's interactive (some .bashrc check [ -z "$PS1" ] && return)
export PS1='$ '

set -a
source -- "$1"
set +a

# Output all environment variables in key=value format
echo "=== ENV_VARS ==="
declare -p | grep 'declare -x' | sed 's/declare -x //'

# Output aliases
echo "=== ALIASES ==="
alias -p 2>/dev/null || true

# Output function names
echo "=== FUNCTIONS ==="
declare -F 2>/dev/null | awk '{{print $3}}' || true

# Output shell options (shopt)
echo "=== SHOPTS ==="
shopt 2>/dev/null || true
"#;

    // Execute bash script to capture the environment, aliases, and functions
    if let Ok(output) = std::process::Command::new("bash")
        .arg("-c")
        .arg(bash_script)
        .arg("jsh-config")
        .arg(path)
        .output()
    {
        let stdout = String::from_utf8_lossy(&output.stdout);
        parse_bash_output(&stdout, state);
    }
}

/// If conda is present after importing bashrc state, load its POSIX shell hook
/// into the current jsh process so `conda activate` works interactively.
fn load_conda_hook(state: &mut ShellState) {
    let conda_cmd = state
        .get_var("CONDA_EXE")
        .filter(|s| !s.is_empty())
        .map(|s| s.to_string())
        .unwrap_or_else(|| "conda".to_string());

    let output = match Command::new(&conda_cmd)
        .args(["shell.posix", "hook"])
        .output()
    {
        Ok(output) if output.status.success() => output,
        _ => return,
    };

    let hook = String::from_utf8_lossy(&output.stdout);
    if hook.trim().is_empty() {
        return;
    }

    if let Ok(commands) = parser::parse(&hook) {
        executor::execute_program(&commands, state);
    }
}

/// Parse bash output containing env vars, aliases, functions, and shopt settings
fn parse_bash_output(output: &str, state: &mut ShellState) {
    let mut current_section = "";

    for line in output.lines() {
        match line {
            "=== ENV_VARS ===" => {
                current_section = "ENV_VARS";
                continue;
            }
            "=== ALIASES ===" => {
                current_section = "ALIASES";
                continue;
            }
            "=== FUNCTIONS ===" => {
                current_section = "FUNCTIONS";
                continue;
            }
            "=== SHOPTS ===" => {
                current_section = "SHOPTS";
                continue;
            }
            _ => {}
        }

        match current_section {
            "ENV_VARS" => {
                if line.is_empty() {
                    continue;
                }
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
            "ALIASES" => {
                if line.is_empty() || !line.starts_with("alias ") {
                    continue;
                }
                // Parse "alias name='value'" format
                let alias_def = &line[6..]; // skip "alias "
                if let Some(eq_pos) = alias_def.find('=') {
                    let name = &alias_def[..eq_pos];
                    let value = &alias_def[eq_pos + 1..];
                    // Remove surrounding quotes
                    let value = value.trim_matches('\'').trim_matches('"');
                    state.aliases.insert(name.to_string(), value.to_string());
                }
            }
            "FUNCTIONS" => {
                if !line.is_empty() {
                    // For now, just note that functions are defined
                    // Full function body parsing requires additional bash invocation
                    // Functions will be stored once we implement full parsing
                }
            }
            "SHOPTS" => {
                if line.is_empty() {
                    continue;
                }
                // Parse shopt output: "shopt_name     on/off"
                let parts: Vec<&str> = line.split_whitespace().collect();
                if parts.len() >= 2 {
                    let opt_name = parts[0];
                    let opt_value = parts[parts.len() - 1];
                    let enabled = opt_value == "on";

                    // Map bash shopt names to jsh ShellOpts
                    match opt_name {
                        "globstar" => state.shell_opts.globstar = enabled,
                        "dotglob" => state.shell_opts.dotglob = enabled,
                        "nullglob" => state.shell_opts.nullglob = enabled,
                        "failglob" => state.shell_opts.failglob = enabled,
                        "extglob" => state.shell_opts.extglob = enabled,
                        "nocaseglob" => state.shell_opts.nocaseglob = enabled,
                        "noglob" => state.shell_opts.noglob = enabled,
                        "lastpipe" => state.shell_opts.lastpipe = enabled,
                        "autocd" => state.shell_opts.autocd = enabled,
                        "cdspell" => state.shell_opts.cdspell = enabled,
                        "checkwinsize" => state.shell_opts.checkwinsize = enabled,
                        "inherit_errexit" => state.shell_opts.inherit_errexit = enabled,
                        _ => {} // Unknown shopt, ignore
                    }
                }
            }
            _ => {}
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::environment::ShellState;

    #[test]
    fn test_parse_bash_output_env_vars() {
        let output = r#"=== ENV_VARS ===
TEST_VAR='hello_world'
MY_PATH='/custom/path'
=== ALIASES ===
=== FUNCTIONS ===
=== SHOPTS ==="#;

        let mut state = ShellState::new(false);
        parse_bash_output(output, &mut state);

        assert_eq!(state.get_var("TEST_VAR"), Some("hello_world"));
        assert_eq!(state.get_var("MY_PATH"), Some("/custom/path"));
    }

    #[test]
    fn test_parse_bash_output_aliases() {
        let output = r#"=== ENV_VARS ===
=== ALIASES ===
alias ll='ls -la'
alias grep='grep --color=auto'
=== FUNCTIONS ===
=== SHOPTS ==="#;

        let mut state = ShellState::new(false);
        parse_bash_output(output, &mut state);

        assert_eq!(state.aliases.get("ll"), Some(&"ls -la".to_string()));
        assert_eq!(
            state.aliases.get("grep"),
            Some(&"grep --color=auto".to_string())
        );
    }

    #[test]
    fn test_parse_bash_output_shopts() {
        let output = r#"=== ENV_VARS ===
=== ALIASES ===
=== FUNCTIONS ===
=== SHOPTS ===
extglob         on
dotglob         off
globstar        on"#;

        let mut state = ShellState::new(false);
        parse_bash_output(output, &mut state);

        assert_eq!(state.shell_opts.extglob, true);
        assert_eq!(state.shell_opts.dotglob, false);
        assert_eq!(state.shell_opts.globstar, true);
    }

    #[test]
    fn test_parse_bash_output_mixed() {
        let output = r#"=== ENV_VARS ===
APP_NAME='myapp'
=== ALIASES ===
alias ll='ls -lah'
=== FUNCTIONS ===
=== SHOPTS ===
extglob         on"#;

        let mut state = ShellState::new(false);
        parse_bash_output(output, &mut state);

        assert_eq!(state.get_var("APP_NAME"), Some("myapp"));
        assert_eq!(state.aliases.get("ll"), Some(&"ls -lah".to_string()));
        assert_eq!(state.shell_opts.extglob, true);
    }

    #[test]
    fn bash_bridge_treats_special_config_path_as_data() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("startup \"$(false)\" file");
        std::fs::write(&path, "export JSH_SPECIAL_RC_PATH=loaded\n").expect("write rc file");

        let mut state = ShellState::new(false);
        source_via_bash(&path, &mut state);

        assert_eq!(state.get_var("JSH_SPECIAL_RC_PATH"), Some("loaded"));
        state.unset_var("JSH_SPECIAL_RC_PATH");
    }

    #[test]
    fn native_config_uses_program_control_flow() {
        let dir = tempfile::tempdir().expect("tempdir");
        let errexit_path = dir.path().join("errexit.jsh");
        std::fs::write(
            &errexit_path,
            "set -e; false; export JSH_AFTER_FAILED_RC=bad\n",
        )
        .expect("write rc file");

        let mut state = ShellState::new(false);
        load_config_file(&errexit_path, &mut state);
        assert_eq!(state.last_exit_code, 1);
        assert_eq!(state.get_var("JSH_AFTER_FAILED_RC"), None);

        let exit_path = dir.path().join("exit.jsh");
        std::fs::write(&exit_path, "exit 7; export JSH_AFTER_EXIT_RC=bad\n")
            .expect("write rc file");
        crate::builtins::reset_exit_request();
        load_config_file(&exit_path, &mut state);
        assert_eq!(state.last_exit_code, 7);
        assert_eq!(state.get_var("JSH_AFTER_EXIT_RC"), None);
        assert!(crate::builtins::EXIT_REQUESTED.load(std::sync::atomic::Ordering::SeqCst));

        let integration = parser::parse("export JSH_AFTER_PREEXISTING_EXIT=bad")
            .expect("parse integration command");
        executor::execute_program(&integration, &mut state);
        assert_eq!(state.last_exit_code, 7);
        assert_eq!(state.get_var("JSH_AFTER_PREEXISTING_EXIT"), None);
        crate::builtins::reset_exit_request();
    }
}
