/// OSC (Operating System Command) escape sequences for terminal integration.
///
/// Supported sequences:
/// - OSC 7:    Report current working directory (file:// URI)
/// - OSC 9:    Desktop notification (Windows Terminal, ConEmu)
/// - OSC 133:  Semantic prompt / shell integration (iTerm2, VS Code, WezTerm, Kitty)
/// - OSC 777:  Terminal notification (iTerm2, Kitty)
/// - OSC 1337: iTerm2 proprietary (CurrentDir)
/// - OSC 0/2:  Window/tab title
///
/// Every packet leaves through `emit`, which is the single place that decides
/// whether the mark sink is a terminal at all, and every value that reaches a
/// packet is escaped and length-bounded first.
use std::env;
use std::io::IsTerminal;

const MAX_OSC_COMMAND_BYTES: usize = 16 * 1024;
const MAX_OSC_CWD_BYTES: usize = crate::execution::MAX_CWD_BYTES;
/// Titles are display-only and terminals truncate them anyway, so keep the
/// packet small: a pathological $PWD or command line must not flood the tty.
const MAX_OSC_TITLE_BYTES: usize = 1024;
/// Matches jterm_core's own notification field cap (MAX_NOTIFICATION_CHARS).
const MAX_OSC_NOTIFICATION_BYTES: usize = 1024;

// ── The single sink ───────────────────────────────────────────
//
// Interactivity in jsh is decided from *stdin* (shell.rs sets
// `state.interactive = stdin_is_tty`), but every mark goes to *stderr*. Nothing
// used to check the sink, so `jsh 2>logfile` filled the log with escape
// sequences and a jsh whose stderr is a pipe still announced prompt marks to
// whatever was reading it.

/// Write one complete OSC packet to the mark sink (stderr) if — and only if —
/// that sink is a terminal. Returns whether the packet was actually written, so
/// callers with an out-of-band fallback (see `job.rs`) can tell.
///
/// Every emitter in this file funnels through here, so no emitter can forget
/// the check — there is only one copy of it to get right.
///
/// The `isatty(2)` is deliberately *not* cached. `exec 2>logfile` can move the
/// sink in the middle of a session, and a cached "stderr is a tty" would keep
/// pouring escapes into that file for the rest of the session. A handful of
/// isatty calls per command is far cheaper than being wrong for hours.
fn emit(packet: &str) -> bool {
    if !std::io::stderr().is_terminal() {
        return false;
    }
    eprint!("{}", packet);
    true
}

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

/// Bound and escape a *display* value — a window title, a filesystem path, a
/// notification string — on its way into an OSC packet.
///
/// The machine-readable OSC 133 fields use `percent_encode_metadata`, which
/// encodes everything outside RFC 3986 unreserved ASCII. That is wrong for
/// display values: it would turn the window title `jsh: ~/dev (main)` into
/// `jsh%3A%20~%2Fdev%20%28main%29`. So printable bytes are passed through and
/// only the bytes that can break *out* of the packet are escaped, using the
/// same `%XX` scheme as the rest of this file:
///
/// * Every terminal-ambiguous character: C0 controls — BEL (0x07) and ESC
///   (0x1B) above all — DEL/C1 controls, non-ASCII spacing, bidi controls,
///   zero-width characters, and other default-ignorables. Protocol controls
///   can terminate the packet and start a fresh sequence; invisible formatting
///   can instead make a command, path, or notification say something different
///   in trusted-looking terminal chrome.
/// * `;` when the value sits in a field-delimited packet (`escape_delimiter`),
///   so it cannot forge an extra field. Values that occupy the packet's final
///   field pass `false`, since a `;` there stays inside that field and dropping
///   the escape keeps human-facing text readable.
fn escape_osc_text(value: &str, max_bytes: usize, escape_delimiter: bool) -> String {
    const HEX: &[u8; 16] = b"0123456789ABCDEF";

    let bounded = bounded_prefix(value, max_bytes);
    let mut escaped = String::with_capacity(bounded.len());
    for ch in bounded.chars() {
        if crate::terminal_text::is_terminal_ambiguous(ch) || (escape_delimiter && ch == ';') {
            let mut buf = [0u8; 4];
            for byte in ch.encode_utf8(&mut buf).as_bytes() {
                escaped.push('%');
                escaped.push(char::from(HEX[usize::from(byte >> 4)]));
                escaped.push(char::from(HEX[usize::from(byte & 0x0f)]));
            }
        } else {
            escaped.push(ch);
        }
    }
    escaped
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
        if let Some(path) = cwd.to_str() {
            if let Some(packet) = cwd_packet(hostname, path) {
                emit(&packet);
            }
        }
    }
}

fn cwd_packet(hostname: &str, path: &str) -> Option<String> {
    if !crate::execution::is_valid_cwd(path) {
        return None;
    }
    let host = percent_encode_metadata(hostname);
    let encoded = percent_encode_path(path);
    Some(format!("\x1b]7;file://{host}{encoded}\x1b\\"))
}

// ── OSC 1337: iTerm2 CurrentDir ───────────────────────────────

/// Emit OSC 1337 CurrentDir for iTerm2.
/// Format: `\x1b]1337;CurrentDir=path\x07`
pub fn report_cwd_iterm2() {
    if let Ok(cwd) = env::current_dir() {
        if let Some(path) = cwd.to_str() {
            if let Some(packet) = cwd_iterm2_packet(path) {
                emit(&packet);
            }
        }
    }
}

/// Build OSC 1337 CurrentDir only when the exact path is safe to send raw.
///
/// Unlike OSC 7's file URI, iTerm2's `CurrentDir` field has no documented
/// escaping. Percent-encoding or truncating a hostile/oversized path would
/// report a different directory, so omit this redundant compatibility frame;
/// [`report_cwd`] remains the canonical, encoded cwd signal.
fn cwd_iterm2_packet(path: &str) -> Option<String> {
    (crate::execution::is_valid_cwd(path) && !path.contains(';'))
        .then(|| format!("\x1b]1337;CurrentDir={path}\x07"))
}

// ── OSC 0/2: Window Title ─────────────────────────────────────

/// Emit OSC 2 to set the window/tab title.
/// Format: `\x1b]2;title\x07`
///
/// Supported by virtually all terminal emulators.
pub fn set_title(title: &str) {
    emit(&set_title_packet(title));
}

/// Build OSC 2. The title is attacker-reachable twice over — shell.rs passes the
/// raw command line, and the idle title is the current directory — so it is
/// escaped and bounded even though the value is meant to be human-readable.
fn set_title_packet(title: &str) -> String {
    format!(
        "\x1b]2;{}\x07",
        escape_osc_text(title, MAX_OSC_TITLE_BYTES, false)
    )
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
    emit("\x1b]133;A\x07");
}

/// Emit OSC 133;B — Prompt end / interactive command input start marker.
/// Call this after rendering the prompt, before accepting editor input.
pub fn command_start() {
    emit("\x1b]133;B\x07");
}

/// Build OSC 133;C with jsh execution metadata.
fn command_output_start_packet(execution_id: &str, command: &str, cwd: &str) -> String {
    let id = percent_encode_metadata(execution_id);
    let mut packet = format!("\x1b]133;C;id={id}");
    if crate::execution::is_valid_command_text(command, MAX_OSC_COMMAND_BYTES) {
        packet.push_str(";cmdline_url=");
        packet.push_str(&percent_encode_metadata(command));
    } else {
        packet.push_str(";cmd_truncated=1");
    }
    if crate::execution::is_valid_cwd(cwd) {
        packet.push_str(";cwd_url=");
        packet.push_str(&percent_encode_metadata(cwd));
    }
    packet.push('\x07');
    packet
}

/// Emit OSC 133;C — Command output start marker with correlation metadata.
/// Call this just before the command's output begins.
pub fn command_output_start(execution_id: &str, command: &str, cwd: &str) {
    emit(&command_output_start_packet(execution_id, command, cwd));
}

/// Build OSC 133;D with the standard positional exit code and jsh metadata.
fn command_finished_packet(
    exit_code: i32,
    execution_id: &str,
    duration_ms: u64,
    cwd: &str,
) -> String {
    let id = percent_encode_metadata(execution_id);
    let mut packet = format!("\x1b]133;D;{exit_code};id={id};duration_ms={duration_ms}");
    if crate::execution::is_valid_cwd(cwd) {
        packet.push_str(";cwd_url=");
        packet.push_str(&percent_encode_metadata(cwd));
    }
    packet.push('\x07');
    packet
}

/// Emit OSC 133;D — Command finished marker with exit code and metadata.
/// Call this after the command completes.
pub fn command_finished(exit_code: i32, execution_id: &str, duration_ms: u64, cwd: &str) {
    emit(&command_finished_packet(
        exit_code,
        execution_id,
        duration_ms,
        cwd,
    ));
}

// ── OSC 7770: jsh Session ID ─────────────────────────────────

/// Emit OSC 7770 to report the jsh session ID to the terminal emulator.
/// Format: `\x1b]7770;session_id\x07`
///
/// This is a custom jsh-specific OSC used by jterm4 to associate
/// a terminal pane with a persistent session.
pub fn report_session_id(session_id: &str) {
    if let Some(packet) = session_id_packet(session_id) {
        emit(&packet);
    }
}

fn session_id_packet(session_id: &str) -> Option<String> {
    // This value is a persistent correlation key, not display text. Escaping
    // or truncating malformed input could transform two distinct caller
    // values into the same valid identifier, so preserve it exactly or emit
    // no frame at all.
    crate::execution::is_valid_session_id(session_id).then(|| format!("\x1b]7770;{session_id}\x07"))
}

// ── OSC 9: Desktop Notification ───────────────────────────────

/// Emit OSC 9 desktop notification.
/// Format: `\x1b]9;message\x07`
///
/// Supported by: Windows Terminal, ConEmu.
///
/// NOT used by the job notifier: every terminal in this family, and every Unix
/// terminal that speaks OSC 9, also speaks OSC 777, so emitting both raises two
/// notifications for one event (jterm_core's parser turns each into the same
/// `ParserEvent::Notification`). Kept for callers that specifically target an
/// OSC-9-only terminal. See `job.rs::send_notification` for the policy.
pub fn notify_osc9(message: &str) -> bool {
    emit(&notify_osc9_packet(message))
}

fn notify_osc9_packet(message: &str) -> String {
    format!(
        "\x1b]9;{}\x07",
        escape_osc_text(message, MAX_OSC_NOTIFICATION_BYTES, false)
    )
}

// ── OSC 777: Terminal Notification ────────────────────────────

/// Emit OSC 777 terminal notification. Returns whether the terminal was there
/// to receive it, so the caller can decide about an out-of-band fallback.
/// Format: `\x1b]777;notify;summary;body\x07`
///
/// Supported by: iTerm2, Kitty, rxvt-unicode, and jterm_core's parser.
pub fn notify_osc777(summary: &str, body: &str) -> bool {
    emit(&notify_osc777_packet(summary, body))
}

/// Build OSC 777. `summary` is escaped including `;` because a forged delimiter
/// there would shift the caller's body into a field the terminal reads as
/// something else; `body` is the packet's last field, so a `;` inside it stays
/// data and is left readable.
fn notify_osc777_packet(summary: &str, body: &str) -> String {
    format!(
        "\x1b]777;notify;{};{}\x07",
        escape_osc_text(summary, MAX_OSC_NOTIFICATION_BYTES, true),
        escape_osc_text(body, MAX_OSC_NOTIFICATION_BYTES, false)
    )
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

        for ambiguous in ["", "echo\rhidden", "echo\x1b[2J", "left\u{202e}right"] {
            assert_eq!(
                command_output_start_packet("jsh-1", ambiguous, "/tmp"),
                "\x1b]133;C;id=jsh-1;cmd_truncated=1;cwd_url=%2Ftmp\x07",
                "command={ambiguous:?}"
            );
        }
    }

    #[test]
    fn cwd_metadata_is_exact_or_omitted_on_a_utf8_boundary() {
        let at_limit = format!("{}雪", "x".repeat(MAX_OSC_CWD_BYTES - 3));
        assert_eq!(at_limit.len(), MAX_OSC_CWD_BYTES);
        let packet = command_output_start_packet("jsh-1", "true", &at_limit);
        assert!(packet.ends_with(&format!(
            ";cwd_url={}{}\x07",
            "x".repeat(MAX_OSC_CWD_BYTES - 3),
            "%E9%9B%AA"
        )));

        let oversized = format!("{}雪", "x".repeat(MAX_OSC_CWD_BYTES - 2));
        assert_eq!(oversized.len(), MAX_OSC_CWD_BYTES + 1);
        assert_eq!(
            command_output_start_packet("jsh-1", "true", &oversized),
            "\x1b]133;C;id=jsh-1;cmdline_url=true\x07"
        );
        assert_eq!(
            command_finished_packet(0, "jsh-1", 7, &oversized),
            "\x1b]133;D;0;id=jsh-1;duration_ms=7\x07"
        );
    }

    /// Reject a packet that carries more than its own framing bytes: one ESC to
    /// open it and one BEL to close it means nothing inside escaped.
    fn assert_single_framing(packet: &str) {
        let bytes = packet.as_bytes();
        assert_eq!(
            bytes.iter().filter(|&&b| b == 0x1b).count(),
            1,
            "{packet:?}"
        );
        assert_eq!(
            bytes.iter().filter(|&&b| b == 0x07).count(),
            1,
            "{packet:?}"
        );
        assert!(
            !packet
                .chars()
                .skip(1)
                .any(|ch| ch.is_control() && ch != '\u{7}'),
            "{packet:?}"
        );
    }

    #[test]
    fn set_title_packet_neutralizes_a_hostile_command_line() {
        // The real vector: a repo you cloned contains a directory (or you type a
        // command) whose name closes the title packet and opens an OSC 52
        // clipboard write, which every terminal in this family implements.
        let packet = set_title_packet("evil\x07\x1b]52;c;aGFjaw==\x07");

        assert_eq!(packet, "\x1b]2;evil%07%1B]52;c;aGFjaw==%07\x07");
        assert_single_framing(&packet);
        assert!(!packet.contains("\x1b]52"));
    }

    #[test]
    fn set_title_packet_escapes_8bit_st_and_del() {
        // 8-bit ST (U+009C) closes an OSC just as ESC \ does, and it hides in a
        // UTF-8 path as 0xC2 0x9C.
        let packet = set_title_packet("a\u{9c}b\u{7f}c");

        assert_eq!(packet, "\x1b]2;a%C2%9Cb%7Fc\x07");
        assert_single_framing(&packet);
    }

    #[test]
    fn display_packets_expose_invisible_and_bidirectional_unicode() {
        let hostile = "left\u{202e}right\u{00a0}tail\u{200b}";
        let visible = "left%E2%80%AEright%C2%A0tail%E2%80%8B";

        let title = set_title_packet(hostile);
        assert_eq!(title, format!("\x1b]2;{visible}\x07"));
        assert_single_framing(&title);

        assert_eq!(cwd_iterm2_packet(&format!("/tmp/{hostile}")), None);

        let notification = notify_osc777_packet(hostile, hostile);
        assert_eq!(
            notification,
            format!("\x1b]777;notify;{visible};{visible}\x07")
        );
        assert_single_framing(&notification);
    }

    #[test]
    fn set_title_packet_keeps_ordinary_titles_readable() {
        // Titles are display values: escaping them wholesale the way the OSC 133
        // metadata fields are escaped would show `jsh%3A%20~%2Fdev` in the tab.
        let packet = set_title_packet("jsh: ~/dev (main) 雪");

        assert_eq!(packet, "\x1b]2;jsh: ~/dev (main) 雪\x07");
        assert_single_framing(&packet);
    }

    #[test]
    fn set_title_packet_bounds_length_on_a_utf8_boundary() {
        let title = format!("{}雪", "x".repeat(MAX_OSC_TITLE_BYTES - 1));
        let packet = set_title_packet(&title);

        assert_eq!(
            packet,
            format!("\x1b]2;{}\x07", "x".repeat(MAX_OSC_TITLE_BYTES - 1))
        );
        assert_single_framing(&packet);
    }

    #[test]
    fn cwd_iterm2_packet_omits_inexact_or_hostile_paths() {
        assert_eq!(
            cwd_iterm2_packet("/tmp/evil\x07\x1b]52;c;aGFjaw==\x07/x;RemoteHost=y"),
            None
        );
        assert_eq!(cwd_iterm2_packet(&"x".repeat(MAX_OSC_CWD_BYTES + 1)), None);
    }

    #[test]
    fn cwd_iterm2_packet_keeps_ordinary_paths_raw() {
        // iTerm2 expects the literal path here, not a URI-encoded one.
        let packet = cwd_iterm2_packet("/home/u/dev/my project").expect("safe exact path");

        assert_eq!(packet, "\x1b]1337;CurrentDir=/home/u/dev/my project\x07");
    }

    #[test]
    fn cwd_packet_encodes_host_and_path() {
        let packet = cwd_packet("host", "/tmp/a b;c雪").expect("safe exact cwd");

        assert_eq!(packet, "\x1b]7;file://host/tmp/a%20b%3Bc%E9%9B%AA\x1b\\");
        // OSC 7 is ST-terminated, so its framing is ESC ] ... ESC \ — two ESC
        // bytes and no BEL of its own.
        assert_eq!(
            packet.as_bytes().iter().filter(|&&b| b == 0x1b).count(),
            2,
            "{packet:?}"
        );
        assert!(packet.ends_with("\x1b\\"));

        assert_eq!(cwd_packet("host", "/tmp/evil\x1b]52;c;x\x07"), None);
        assert_eq!(cwd_packet("host", ""), None);
        assert_eq!(cwd_packet("host", &"x".repeat(MAX_OSC_CWD_BYTES + 1)), None);
    }

    #[test]
    fn notification_packets_cannot_escape_their_frame() {
        let osc777 = notify_osc777_packet(
            "Command failed (exit 1)\x07\x1b]52;c;x\x07",
            "make -j8; echo done\x1b]0;pwned\x07 (3.0s)",
        );

        assert_eq!(
            osc777,
            "\x1b]777;notify;Command failed (exit 1)%07%1B]52%3Bc%3Bx%07;make -j8; echo done%1B]0;pwned%07 (3.0s)\x07"
        );
        assert_single_framing(&osc777);

        let osc9 = notify_osc9_packet("done\x07\x1b]52;c;x\x07");
        assert_eq!(osc9, "\x1b]9;done%07%1B]52;c;x%07\x07");
        assert_single_framing(&osc9);
    }

    #[test]
    fn session_id_packet_keeps_valid_ids_exact_and_rejects_invalid_ids() {
        assert_eq!(
            session_id_packet("jsh-cbf29ce484222325-1c3a36-19fb219b03a"),
            Some("\x1b]7770;jsh-cbf29ce484222325-1c3a36-19fb219b03a\x07".to_string())
        );

        assert_eq!(session_id_packet(""), None);
        for hostile in [
            "id\x07\x1b]52;c;x\x07",
            "id;other",
            "id.with-dot",
            "id%2Descaped",
            "会话",
        ] {
            assert_eq!(session_id_packet(hostile), None, "id={hostile:?}");
        }
    }

    #[test]
    fn session_id_packet_never_truncates_distinct_oversized_ids_to_one_key() {
        let common_prefix = "a".repeat(crate::execution::MAX_SESSION_ID_BYTES);
        let first = format!("{common_prefix}x");
        let second = format!("{common_prefix}y");

        assert_eq!(common_prefix.len(), crate::execution::MAX_SESSION_ID_BYTES);
        assert_eq!(
            session_id_packet(&common_prefix),
            Some(format!("\x1b]7770;{common_prefix}\x07"))
        );
        assert_eq!(first.len(), crate::execution::MAX_SESSION_ID_BYTES + 1);
        assert_eq!(session_id_packet(&first), None);
        assert_eq!(session_id_packet(&second), None);
    }

    #[test]
    fn emission_follows_the_sink_being_a_terminal() {
        // The invariant, whatever the test runner's stderr happens to be: a mark
        // is written exactly when the sink is a tty, and `emit` reports what it
        // did so callers with a fallback (job.rs) can act on it. When stderr is
        // a file — `jsh 2>logfile` — nothing is written at all.
        let sink_is_terminal = std::io::stderr().is_terminal();
        assert_eq!(emit("\x1b]133;A\x07"), sink_is_terminal);
        assert_eq!(notify_osc777("summary", "body"), sink_is_terminal);
    }

    #[test]
    fn command_finished_keeps_positional_exit_and_encodes_metadata() {
        let packet = command_finished_packet(127, "jsh;2", 42, "/tmp/a;b雪");

        assert_eq!(
            packet,
            "\x1b]133;D;127;id=jsh%3B2;duration_ms=42;cwd_url=%2Ftmp%2Fa%3Bb%E9%9B%AA\x07"
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

        assert_eq!(
            command_finished_packet(127, "jsh;2", 42, "/tmp/\x1b]133;A\x07"),
            "\x1b]133;D;127;id=jsh%3B2;duration_ms=42\x07"
        );
    }
}
