//! Shared classification for text that is unsafe to render verbatim in a
//! terminal UI. C0/C1 controls can execute terminal protocols; Unicode format
//! characters can reorder or conceal security-relevant text without using an
//! escape byte.

pub(crate) fn is_terminal_ambiguous(ch: char) -> bool {
    ch.is_control()
        // Non-ASCII separators and no-break spaces can look like an ordinary
        // space or line break while the shell treats them as command data.
        // Keep the ordinary ASCII space readable; callers already decide how
        // to render structural newline/tab characters.
        || (ch.is_whitespace() && ch != ' ')
        || matches!(
            u32::from(ch),
            0x00ad
                | 0x034f
                | 0x061c
                | 0x115f..=0x1160
                | 0x17b4..=0x17b5
                | 0x180b..=0x180f
                | 0x200b..=0x200f
                | 0x202a..=0x202e
                | 0x2060..=0x206f
                | 0x3164
                | 0xfe00..=0xfe0f
                | 0xfeff
                | 0xffa0
                | 0xfff0..=0xfffb
                | 0x1bca0..=0x1bca3
                | 0x1d173..=0x1d17a
                | 0xe0000..=0xe0fff
        )
}

/// Render untrusted text on one terminal line without forwarding terminal
/// protocols or visually ambiguous Unicode. The result is capped in bytes on
/// a UTF-8 boundary; an ellipsis marks truncation when it fits.
pub(crate) fn escape_inline(value: &str, max_bytes: usize) -> String {
    let mut output = String::new();
    for ch in value.chars() {
        let rendered = match ch {
            '\n' => "\\n".to_string(),
            '\r' => "\\r".to_string(),
            '\t' => "\\t".to_string(),
            ch if is_terminal_ambiguous(ch) && u32::from(ch) <= 0x7f => {
                format!("\\x{:02x}", u32::from(ch))
            }
            ch if is_terminal_ambiguous(ch) => format!("\\u{{{:x}}}", u32::from(ch)),
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

/// Whether a value is safe to place on a single editable command line. This
/// intentionally rejects every character that [`escape_inline`] would expand:
/// displaying an escaped spelling but later executing the hidden original
/// would break exact-review semantics.
pub(crate) fn is_safe_inline(value: &str) -> bool {
    !value.chars().any(is_terminal_ambiguous)
}

#[cfg(test)]
mod tests {
    use super::{escape_inline, is_safe_inline, is_terminal_ambiguous};

    #[test]
    fn width_tables_track_unicode_17() {
        assert_eq!(unicode_width::UNICODE_VERSION, (17, 0, 0));
    }

    #[test]
    fn classifies_protocol_controls_and_invisible_formatting() {
        for ch in [
            '\x1b',
            '\u{0085}',
            '\u{00a0}',
            '\u{00ad}',
            '\u{200b}',
            '\u{2028}',
            '\u{202e}',
            '\u{2066}',
            '\u{1bca0}',
            '\u{1d173}',
            '\u{fff0}',
            '\u{e0000}',
            '\u{e0020}',
            '\u{e0fff}',
        ] {
            assert!(is_terminal_ambiguous(ch), "missed U+{:04X}", u32::from(ch));
        }
        for ch in ['a', '雪', ' ', '-', '\n'] {
            assert_eq!(is_terminal_ambiguous(ch), ch == '\n');
        }
    }

    #[test]
    fn inline_rendering_is_lossless_for_safe_text_and_escapes_hostile_text() {
        assert_eq!(escape_inline("hello 雪", 64), "hello 雪");
        assert_eq!(
            escape_inline("a\x1b\u{202e}\u{e0020}\nb", 128),
            "a\\x1b\\u{202e}\\u{e0020}\\nb"
        );
        assert!(is_safe_inline("printf '雪'"));
        assert!(!is_safe_inline("printf\u{202e} 'snow'"));

        let bounded = escape_inline(&"雪".repeat(64), 17);
        assert!(bounded.len() <= 17);
        assert!(bounded.is_char_boundary(bounded.len()));
    }
}
