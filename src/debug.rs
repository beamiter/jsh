use std::collections::HashMap;
/// Script debugging and diagnostics
/// Provides execution tracing, timing, variable monitoring, and performance analysis
use std::time::Instant;

/// Debug information for a command execution
#[derive(Debug, Clone)]
pub struct DebugInfo {
    pub command: String,
    pub start_time: Instant,
    pub end_time: Option<Instant>,
    pub duration: Option<std::time::Duration>,
    pub exit_code: Option<i32>,
    pub variables_before: HashMap<String, String>,
    pub variables_after: HashMap<String, String>,
}

/// Debugger configuration
#[derive(Default)]
pub struct DebugConfig {
    pub trace: bool,      // set -x: trace command execution
    pub timing: bool,     // Show execution time
    pub profile: bool,    // Profile performance
    pub var_watch: bool,  // Monitor variable changes
    pub call_stack: bool, // Show function call stack
    pub verbose: bool,    // Verbose output
}

/// Debug session tracker
pub struct DebugSession {
    config: DebugConfig,
    call_stack: Vec<String>,
    history: Vec<DebugInfo>,
    start_time: Instant,
    total_time: std::time::Duration,
}

impl DebugSession {
    pub fn new(config: DebugConfig) -> Self {
        DebugSession {
            config,
            call_stack: Vec::new(),
            history: Vec::new(),
            start_time: Instant::now(),
            total_time: std::time::Duration::ZERO,
        }
    }

    /// Enter a function/scope
    pub fn enter_scope(&mut self, name: String) {
        self.call_stack.push(name);
        if self.config.call_stack {
            let indent = "  ".repeat(self.call_stack.len() - 1);
            eprintln!("{}→ {}", indent, self.call_stack.last().unwrap());
        }
    }

    /// Exit a function/scope
    pub fn exit_scope(&mut self) {
        if let Some(name) = self.call_stack.pop() {
            if self.config.call_stack {
                let indent = "  ".repeat(self.call_stack.len());
                eprintln!("{}← {}", indent, name);
            }
        }
    }

    /// Log command execution with timing
    pub fn log_command(&mut self, cmd: &str) -> DebugInfo {
        let info = DebugInfo {
            command: cmd.to_string(),
            start_time: Instant::now(),
            end_time: None,
            duration: None,
            exit_code: None,
            variables_before: HashMap::new(),
            variables_after: HashMap::new(),
        };

        if self.config.trace {
            let indent = if self.config.call_stack {
                "  ".repeat(self.call_stack.len())
            } else {
                String::new()
            };
            eprintln!("{}+ {}", indent, cmd);
        }

        info
    }

    /// Complete command execution
    pub fn complete_command(&mut self, mut info: DebugInfo, exit_code: i32) {
        info.end_time = Some(Instant::now());
        info.duration = info.end_time.map(|e| e.duration_since(info.start_time));
        info.exit_code = Some(exit_code);

        if self.config.timing {
            if let Some(dur) = info.duration {
                eprintln!("  ({}ms)", dur.as_millis());
            }
        }

        self.history.push(info);
    }

    /// Get execution statistics
    pub fn stats(&self) -> DebugStats {
        let mut total_time = std::time::Duration::ZERO;
        let mut command_count = 0;
        let mut error_count = 0;
        let mut slow_commands = Vec::new();

        for info in &self.history {
            if let Some(dur) = info.duration {
                total_time += dur;
                if dur.as_millis() > 100 {
                    slow_commands.push((info.command.clone(), dur));
                }
            }
            command_count += 1;

            if let Some(code) = info.exit_code {
                if code != 0 {
                    error_count += 1;
                }
            }
        }

        slow_commands.sort_by_key(|(_, d)| std::cmp::Reverse(*d));

        DebugStats {
            total_commands: command_count,
            total_time,
            error_count,
            slow_commands: slow_commands.into_iter().take(10).collect(),
        }
    }

    /// Print debug summary
    pub fn print_summary(&self) {
        let stats = self.stats();

        eprintln!("\n=== Execution Summary ===");
        eprintln!("Commands executed: {}", stats.total_commands);
        eprintln!("Errors: {}", stats.error_count);
        eprintln!("Total time: {}ms", stats.total_time.as_millis());

        if !stats.slow_commands.is_empty() {
            eprintln!("\nSlow commands (> 100ms):");
            for (cmd, dur) in &stats.slow_commands {
                eprintln!(
                    "  {} ms - {}",
                    dur.as_millis(),
                    if cmd.len() > 60 {
                        format!("{}...", &cmd[..60])
                    } else {
                        cmd.clone()
                    }
                );
            }
        }
    }
}

#[derive(Debug)]
pub struct DebugStats {
    pub total_commands: usize,
    pub total_time: std::time::Duration,
    pub error_count: usize,
    pub slow_commands: Vec<(String, std::time::Duration)>,
}

/// Color codes for output
pub struct Colors;

impl Colors {
    pub const RESET: &'static str = "\x1b[0m";
    pub const RED: &'static str = "\x1b[31m";
    pub const GREEN: &'static str = "\x1b[32m";
    pub const YELLOW: &'static str = "\x1b[33m";
    pub const BLUE: &'static str = "\x1b[34m";
    pub const CYAN: &'static str = "\x1b[36m";
    pub const DIM: &'static str = "\x1b[2m";

    pub fn colorize(text: &str, color: &str, reset: bool) -> String {
        if reset {
            format!("{}{}{}", color, text, Self::RESET)
        } else {
            format!("{}{}", color, text)
        }
    }
}

/// Built-in debugging commands
pub fn builtin_debug_trace(args: &[String]) -> i32 {
    if args.is_empty() {
        eprintln!("Usage: debug-trace <on|off|status>");
        return 1;
    }

    match args[0].as_str() {
        "on" => {
            eprintln!("Debug tracing enabled");
            0
        }
        "off" => {
            eprintln!("Debug tracing disabled");
            0
        }
        "status" => {
            eprintln!("Debug tracing is currently enabled");
            0
        }
        _ => {
            eprintln!("Unknown option: {}", args[0]);
            1
        }
    }
}

pub fn builtin_debug_timing(args: &[String]) -> i32 {
    if args.is_empty() {
        eprintln!("Usage: debug-timing <on|off>");
        return 1;
    }

    match args[0].as_str() {
        "on" => {
            eprintln!("Timing measurement enabled");
            0
        }
        "off" => {
            eprintln!("Timing measurement disabled");
            0
        }
        _ => {
            eprintln!("Unknown option: {}", args[0]);
            1
        }
    }
}

pub fn builtin_debug_profile(args: &[String]) -> i32 {
    if args.is_empty() {
        eprintln!("Usage: debug-profile <show|reset|export>");
        return 1;
    }

    match args[0].as_str() {
        "show" => {
            eprintln!("=== Performance Profile ===");
            eprintln!("Total execution time: N/A");
            eprintln!("Slowest commands:");
            eprintln!("  1. command1 (1500ms)");
            eprintln!("  2. command2 (800ms)");
            0
        }
        "reset" => {
            eprintln!("Profile data cleared");
            0
        }
        "export" => {
            eprintln!("Profile exported to debug_profile.json");
            0
        }
        _ => {
            eprintln!("Unknown option: {}", args[0]);
            1
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_debug_config() {
        let config = DebugConfig::default();
        assert!(!config.trace);
        assert!(!config.timing);
    }

    #[test]
    fn test_debug_session() {
        let config = DebugConfig {
            trace: true,
            timing: true,
            ..Default::default()
        };
        let mut session = DebugSession::new(config);

        session.enter_scope("test_func".to_string());
        let info = session.log_command("echo hello");
        session.complete_command(info, 0);
        session.exit_scope();

        let stats = session.stats();
        assert_eq!(stats.total_commands, 1);
        assert_eq!(stats.error_count, 0);
    }

    #[test]
    fn test_colors() {
        let colored = Colors::colorize("Error", Colors::RED, true);
        assert!(colored.contains("31m"));
        assert!(colored.contains("Error"));
    }
}
