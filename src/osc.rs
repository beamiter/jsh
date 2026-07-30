/// OSC (Operating System Command) escape sequences for terminal integration.
///
/// Supported sequences:
/// - OSC 7:    Report current working directory (file:// URI)
/// - OSC 9:    Desktop notification (Windows Terminal, ConEmu)
/// - OSC 133:  Semantic prompt / shell integration (iTerm2, VS Code, WezTerm, Kitty)
/// - OSC 777:  Terminal notification (iTerm2, Kitty)
/// - OSC 1337: iTerm2 proprietary (CurrentDir)
/// - OSC 0/2:  Window/tab title
use std::env;

const MAX_OSC_COMMAND_BYTES: usize = 16 * 1024;
const MAX_OSC_CWD_BYTES: usize = 4 * 1024;

/// Percent-encode a path for use in file:// URIs (OSC 7).
/// Encodes everything except unreserved characters and `/`.
fn percent_encode_path(path: &str) -> String {
    let mut encoded = String::with_capacity(path.len());
    for byte in path.bytes() {
        match byte {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'/' | b'.' | b'-' | b'_' | b'~' => {
                encoded.push(byte as char);
            }
            _ => {
                encoded.push_str(&format!("%{:02X}", byte));
            }
        }
    }
    encoded
}

/// Percent-encode an OSC metadata value. Only RFC 3986 unreserved ASCII is
/// emitted verbatim, so field delimiters and terminal control bytes can never
/// escape into the surrounding OSC packet.
fn percent_encode_metadata(value: &str) -> String {
    const HEX: &[u8; 16] = b"0123456789ABCDEF";

    let mut encoded = String::with_capacity(value.len());
    for byte in value.bytes() {
        if matches!(
            byte,
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'.' | b'-' | b'_' | b'~'
        ) {
            encoded.push(char::from(byte));
        } else {
            encoded.push('%');
            encoded.push(char::from(HEX[usize::from(byte >> 4)]));
            encoded.push(char::from(HEX[usize::from(byte & 0x0f)]));
        }
    }
    encoded
}

fn bounded_prefix(value: &str, max_bytes: usize) -> &str {
    if value.len() <= max_bytes {
        return value;
    }

    let mut end = max_bytes;
    while !value.is_char_boundary(end) {
        end -= 1;
    }
    &value[..end]
}

// ── OSC 7: Current Working Directory ──────────────────────────

/// Emit OSC 7 to report the current working directory to the terminal.
/// Format: `\x1b]7;file://hostname/path\x1b\\`
///
/// Supported by: iTerm2, WezTerm, Kitty, foot, GNOME Terminal, Windows Terminal.
pub fn report_cwd(hostname: &str) {
    if let Ok(cwd) = env::current_dir() {
        let encoded = percent_encode_path(&cwd.to_string_lossy());
        eprint!("\x1b]7;file://{}{}\x1b\\", hostname, encoded);
    }
}

// ── OSC 1337: iTerm2 CurrentDir ───────────────────────────────

/// Emit OSC 1337 CurrentDir for iTerm2.
/// Format: `\x1b]1337;CurrentDir=path\x07`
pub fn report_cwd_iterm2() {
    if let Ok(cwd) = env::current_dir() {
        eprint!("\x1b]1337;CurrentDir={}\x07", cwd.to_string_lossy());
    }
}

// ── OSC 0/2: Window Title ─────────────────────────────────────

/// Emit OSC 2 to set the window/tab title.
/// Format: `\x1b]2;title\x07`
///
/// Supported by virtually all terminal emulators.
pub fn set_title(title: &str) {
    eprint!("\x1b]2;{}\x07", title);
}

// ── OSC 133: Semantic Prompt (Shell Integration) ──────────────
//
// These markers allow terminals to understand the structure of
// shell interaction: where the prompt is, where user input ends,
// where command output begins and ends. This enables features like
// click-to-jump between prompts, select command output, scroll to
// previous command, and per-command exit status indicators.
//
// Lifecycle per command:
//   133;A  →  [prompt displayed]  →  133;B  →  [user types]
//          →  133;C  →  [command output]  →  133;D;exitcode
//
// Supported by: iTerm2, VS Code terminal, WezTerm, Kitty, foot.

/// Emit OSC 133;A — Prompt start marker.
/// Call this immediately before rendering the prompt.
pub fn prompt_start() {
    eprint!("\x1b]133;A\x07");
}

/// Emit OSC 133;B — Prompt end / interactive command input start marker.
/// Call this after rendering the prompt, before accepting editor input.
pub fn command_start() {
    eprint!("\x1b]133;B\x07");
}

/// Build OSC 133;C with jsh execution metadata.
fn command_output_start_packet(execution_id: &str, command: &str, cwd: &str) -> String {
    let id = percent_encode_metadata(execution_id);
    let cwd = percent_encode_metadata(bounded_prefix(cwd, MAX_OSC_CWD_BYTES));
    let mut packet = format!("\x1b]133;C;id={id}");
    if command.len() <= MAX_OSC_COMMAND_BYTES {
        packet.push_str(";cmdline_url=");
        packet.push_str(&percent_encode_metadata(command));
    } else {
        packet.push_str(";cmd_truncated=1");
    }
    packet.push_str(";cwd_url=");
    packet.push_str(&cwd);
    packet.push('\x07');
    packet
}

/// Emit OSC 133;C — Command output start marker with correlation metadata.
/// Call this just before the command's output begins.
pub fn command_output_start(execution_id: &str, command: &str, cwd: &str) {
    eprint!(
        "{}",
        command_output_start_packet(execution_id, command, cwd)
    );
}

/// Build OSC 133;D with the standard positional exit code and jsh metadata.
fn command_finished_packet(
    exit_code: i32,
    execution_id: &str,
    duration_ms: u64,
    cwd: &str,
) -> String {
    let id = percent_encode_metadata(execution_id);
    let cwd = percent_encode_metadata(bounded_prefix(cwd, MAX_OSC_CWD_BYTES));
    format!("\x1b]133;D;{exit_code};id={id};duration_ms={duration_ms};cwd_url={cwd}\x07")
}

/// Emit OSC 133;D — Command finished marker with exit code and metadata.
/// Call this after the command completes.
pub fn command_finished(exit_code: i32, execution_id: &str, duration_ms: u64, cwd: &str) {
    eprint!(
        "{}",
        command_finished_packet(exit_code, execution_id, duration_ms, cwd)
    );
}

// ── OSC 7770: jsh Session ID ─────────────────────────────────

/// Emit OSC 7770 to report the jsh session ID to the terminal emulator.
/// Format: `\x1b]7770;session_id\x07`
///
/// This is a custom jsh-specific OSC used by jterm4 to associate
/// a terminal pane with a persistent session.
pub fn report_session_id(session_id: &str) {
    eprint!("\x1b]7770;{}\x07", session_id);
}

// ── OSC 9: Desktop Notification ───────────────────────────────

/// Emit OSC 9 desktop notification.
/// Format: `\x1b]9;message\x07`
///
/// Supported by: Windows Terminal, ConEmu.
pub fn notify_osc9(message: &str) {
    eprint!("\x1b]9;{}\x07", message);
}

// ── OSC 777: Terminal Notification ────────────────────────────

/// Emit OSC 777 terminal notification.
/// Format: `\x1b]777;notify;summary;body\x07`
///
/// Supported by: iTerm2, Kitty, rxvt-unicode.
pub fn notify_osc777(summary: &str, body: &str) {
    eprint!("\x1b]777;notify;{};{}\x07", summary, body);
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn command_start_packet_percent_encodes_exact_metadata() {
        let packet =
            command_output_start_packet("jsh:7;\x1b\x07", "printf 'a;b+c'\n雪", "/tmp/a;b%雪");

        assert_eq!(
            packet,
            "\x1b]133;C;id=jsh%3A7%3B%1B%07;cmdline_url=printf%20%27a%3Bb%2Bc%27%0A%E9%9B%AA;cwd_url=%2Ftmp%2Fa%3Bb%25%E9%9B%AA\x07"
        );
        assert_eq!(
            packet
                .as_bytes()
                .iter()
                .filter(|&&byte| byte == 0x1b)
                .count(),
            1
        );
        assert_eq!(
            packet
                .as_bytes()
                .iter()
                .filter(|&&byte| byte == 0x07)
                .count(),
            1
        );
    }

    #[test]
    fn command_start_packet_omits_oversized_command() {
        let at_limit = "x".repeat(MAX_OSC_COMMAND_BYTES);
        let included = command_output_start_packet("jsh-1", &at_limit, "/tmp");
        assert!(included.contains(";cmdline_url="));
        assert!(!included.contains("cmd_truncated"));

        let over_limit = "x".repeat(MAX_OSC_COMMAND_BYTES + 1);
        let omitted = command_output_start_packet("jsh-1", &over_limit, "/tmp");
        assert_eq!(
            omitted,
            "\x1b]133;C;id=jsh-1;cmd_truncated=1;cwd_url=%2Ftmp\x07"
        );
        assert!(!omitted.contains(&over_limit));
    }

    #[test]
    fn packets_bound_cwd_on_a_utf8_boundary() {
        let cwd = format!("{}雪", "x".repeat(MAX_OSC_CWD_BYTES - 1));
        let packet = command_output_start_packet("jsh-1", "true", &cwd);

        assert!(packet.ends_with(&format!(
            ";cwd_url={}\x07",
            "x".repeat(MAX_OSC_CWD_BYTES - 1)
        )));
    }

    #[test]
    fn command_finished_keeps_positional_exit_and_encodes_metadata() {
        let packet = command_finished_packet(127, "jsh;2", 42, "/tmp/\x1b]133;A\x07");

        assert_eq!(
            packet,
            "\x1b]133;D;127;id=jsh%3B2;duration_ms=42;cwd_url=%2Ftmp%2F%1B%5D133%3BA%07\x07"
        );
        assert_eq!(
            packet
                .as_bytes()
                .iter()
                .filter(|&&byte| byte == 0x1b)
                .count(),
            1
        );
        assert_eq!(
            packet
                .as_bytes()
                .iter()
                .filter(|&&byte| byte == 0x07)
                .count(),
            1
        );
    }
}
