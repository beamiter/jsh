//! Reading the command line: where one command ends and the next begins,
//! which word the cursor is in, and what that word's quoting means.
//!
//! Everything here works on byte offsets into the buffer the editor holds,
//! and every offset returned has to be a character boundary that buffer can
//! be sliced at — the editor replaces a range with the chosen candidate, and
//! a wrong offset there is a panic on a keystroke.

use std::collections::HashMap;

/// The command an alias stands for, following one-word aliases and taking
/// the head word of a multi-word one.
pub fn resolve_alias(cmd: &str, aliases: &HashMap<String, String>) -> String {
    let expanded = alias_expanded_segment(cmd, aliases);
    expanded
        .split_whitespace()
        .next()
        .unwrap_or(cmd)
        .to_string()
}

/// A one-word alias is a transparent command wrapper for completion (`g=git`).
pub(super) fn resolve_transparent_alias(
    mut cmd: String,
    aliases: &HashMap<String, String>,
) -> String {
    for _ in 0..8 {
        let Some(expansion) = aliases.get(&cmd) else {
            break;
        };
        let mut words = expansion.split_whitespace();
        let Some(target) = words.next() else {
            break;
        };
        if words.next().is_some() || target == cmd {
            break;
        }
        cmd = target.to_string();
    }
    cmd
}

/// The active command with its leading alias expanded, as the words a
/// completion should reason about.
///
/// `alias gs='git status'` makes `gs -<TAB>` a `git status` flag position,
/// not the first argument of a command called `gs`. Only the head word is
/// substituted, and only while it keeps naming an alias, so the expansion of
/// a multi-word alias contributes its own subcommands and options exactly as
/// if they had been typed.
pub(super) fn alias_expanded_segment(segment: &str, aliases: &HashMap<String, String>) -> String {
    let mut expanded = segment.to_string();
    for _ in 0..8 {
        let words: Vec<&str> = expanded.split_whitespace().collect();
        let index = effective_command_index(&words);
        let Some(head) = words.get(index).copied() else {
            break;
        };
        let Some(expansion) = aliases.get(head) else {
            break;
        };
        // `alias ls='ls --color=auto'` is the idiomatic self-reference: it
        // expands once and then stands for the real command, exactly as the
        // shell itself resolves it.
        let self_referential = expansion.split_whitespace().next() == Some(head);
        let Some(offset) = word_offset(&expanded, index) else {
            break;
        };
        expanded.replace_range(offset..offset + head.len(), expansion);
        if self_referential {
            break;
        }
    }
    expanded
}

/// Byte offset of the nth whitespace-separated word.
pub(super) fn word_offset(text: &str, index: usize) -> Option<usize> {
    text.split_whitespace()
        .nth(index)
        .map(|word| word.as_ptr() as usize - text.as_ptr() as usize)
}

pub(super) fn first_command(buf: &str) -> String {
    command_words(active_command_segment(buf))
        .next()
        .unwrap_or("")
        .to_string()
}

pub(super) fn command_words(segment: &str) -> impl Iterator<Item = &str> {
    let words: Vec<&str> = segment.split_whitespace().collect();
    let command_index = effective_command_index(&words);
    words.into_iter().skip(command_index)
}

pub(super) fn effective_command_index(words: &[&str]) -> usize {
    let mut index = 0;
    loop {
        while index < words.len() && is_assignment_word(words[index]) {
            index += 1;
        }
        let Some(wrapper) = words.get(index).copied() else {
            return index;
        };
        match wrapper {
            "sudo" => {
                index += 1;
                index = skip_wrapper_options(
                    words,
                    index,
                    &[
                        "-u",
                        "--user",
                        "-g",
                        "--group",
                        "-h",
                        "--host",
                        "-p",
                        "--prompt",
                        "-C",
                        "--close-from",
                        "-T",
                        "--command-timeout",
                        "-R",
                        "--chroot",
                        "-D",
                        "--chdir",
                    ],
                );
            }
            "env" => {
                index += 1;
                index = skip_wrapper_options(
                    words,
                    index,
                    &["-u", "--unset", "-C", "--chdir", "-S", "--split-string"],
                );
            }
            "command" | "builtin" | "nohup" => {
                index += 1;
                index = skip_wrapper_options(words, index, &[]);
            }
            "exec" | "time" => {
                index += 1;
                index = skip_wrapper_options(words, index, &["-a", "-f", "-o"]);
            }
            "nice" => {
                index += 1;
                index = skip_wrapper_options(words, index, &["-n", "--adjustment"]);
            }
            // Keywords a command follows. `while read x; do gr<TAB>` is a
            // command position, not an argument of `do`.
            "do" | "then" | "else" | "elif" | "if" | "while" | "until" | "!" | "{" => {
                index += 1;
            }
            _ => return index,
        }
    }
}

pub(super) fn skip_wrapper_options(
    words: &[&str],
    mut index: usize,
    value_options: &[&str],
) -> usize {
    while let Some(word) = words.get(index).copied() {
        if word == "--" {
            return index + 1;
        }
        if is_assignment_word(word) {
            index += 1;
            continue;
        }
        if !word.starts_with('-') || word == "-" {
            break;
        }
        let option = word.split_once('=').map(|(name, _)| name).unwrap_or(word);
        index += 1;
        if !word.contains('=') && value_options.contains(&option) {
            index = (index + 1).min(words.len());
        }
    }
    index
}

/// Is the word being completed the target of a redirection? True for the
/// operators that name a file (`>`, `>>`, `<`, `2>`, `&>`, `<>`), false for
/// `>&`/`<&`, whose operand is a file descriptor.
pub(super) fn is_redirect_target(before: &str) -> bool {
    let before = before.trim_end_matches([' ', '\t']);
    if before.ends_with('&') {
        return false;
    }
    let mut quote = None;
    let mut escaped = false;
    let mut last_operator = false;
    for ch in before.chars() {
        if escaped {
            escaped = false;
            last_operator = false;
            continue;
        }
        match quote {
            Some('\'') => {
                if ch == '\'' {
                    quote = None;
                }
            }
            Some('"') => match ch {
                '"' => quote = None,
                '\\' => escaped = true,
                _ => {}
            },
            _ => match ch {
                '\\' => escaped = true,
                '\'' | '"' => {
                    quote = Some(ch);
                    last_operator = false;
                }
                '<' | '>' => last_operator = true,
                ch if ch.is_whitespace() => {}
                _ => last_operator = false,
            },
        }
    }
    last_operator
}

/// An unclosed quote opening the word being completed, with the text inside
/// it. `cat "my fi` completes inside the quotes rather than replacing them
/// with backslash escapes.
pub(super) fn open_quote_context(word: &str) -> Option<(char, &str)> {
    let quote = word.chars().next().filter(|ch| *ch == '\'' || *ch == '"')?;
    let inner = &word[quote.len_utf8()..];
    // A quote the user already closed is not an open context; the word after
    // it is ordinary text again.
    let closed = match quote {
        '\'' => inner.contains('\''),
        _ => {
            let mut escaped = false;
            inner.chars().any(|ch| {
                let closes = !escaped && ch == '"';
                escaped = !escaped && ch == '\\';
                closes
            })
        }
    };
    (!closed).then_some((quote, inner))
}

/// Byte offset of the value in a `NAME=value` or `NAME+=value` word, if the
/// word is a well-formed shell assignment.
pub(super) fn assignment_value_start(word: &str) -> Option<usize> {
    let eq = word.find('=')?;
    let name = word[..eq].strip_suffix('+').unwrap_or(&word[..eq]);
    let mut chars = name.chars();
    let valid = matches!(chars.next(), Some(ch) if ch == '_' || ch.is_ascii_alphabetic())
        && chars.all(|ch| ch == '_' || ch.is_ascii_alphanumeric());
    valid.then_some(eq + 1)
}

pub(super) fn is_assignment_word(word: &str) -> bool {
    let Some((name, _)) = word.split_once('=') else {
        return false;
    };
    let mut chars = name.chars();
    matches!(chars.next(), Some(ch) if ch == '_' || ch.is_ascii_alphabetic())
        && chars.all(|ch| ch == '_' || ch.is_ascii_alphanumeric())
}

pub(crate) fn active_command_segment(buf: &str) -> &str {
    let start = active_command_segment_start(buf);
    buf[start..].trim_start()
}

pub(super) fn active_command_segment_start(buf: &str) -> usize {
    // Each parenthesis level tracks its own most recent command separator, so
    // separators inside `$(...)` do not leak into the outer command after `)`.
    let mut starts = vec![0usize];
    let mut quote = None;
    let mut escaped = false;
    let chars: Vec<(usize, char)> = buf.char_indices().collect();
    for (position, (index, ch)) in chars.iter().copied().enumerate() {
        if escaped {
            escaped = false;
            continue;
        }
        match quote {
            Some('\'') => {
                if ch == '\'' {
                    quote = None;
                }
            }
            Some('"') => match ch {
                '"' => quote = None,
                '\\' => escaped = true,
                _ => {}
            },
            _ => match ch {
                '\\' => escaped = true,
                '\'' | '"' => quote = Some(ch),
                '(' => starts.push(index + ch.len_utf8()),
                ')' if starts.len() > 1 => {
                    starts.pop();
                }
                // A backtick opens a command substitution and the next one
                // closes it, so each is the start of a fresh command.
                '`' => {
                    *starts.last_mut().unwrap() = index + ch.len_utf8();
                }
                ';' | '\n' | '|' => {
                    *starts.last_mut().unwrap() = index + ch.len_utf8();
                }
                '&' => {
                    let previous = position.checked_sub(1).map(|p| chars[p].1);
                    let next = chars.get(position + 1).map(|(_, ch)| *ch);
                    // `&>` and `>&` are redirections, not command separators.
                    if previous != Some('>') && next != Some('>') {
                        *starts.last_mut().unwrap() = index + ch.len_utf8();
                    }
                }
                _ => {}
            },
        }
    }
    *starts.last().unwrap_or(&0)
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum WrapperValueKind {
    User,
    Group,
}

/// Is the word being completed the value of a wrapper option that names a
/// user or group (`sudo -u <TAB>`)? Only while still inside sudo's own option
/// zone — once the wrapped command starts, its flags are its own.
pub(super) fn wrapper_value_kind(before: &str) -> Option<WrapperValueKind> {
    let segment = active_command_segment(before);
    let words: Vec<&str> = segment.split_whitespace().collect();
    let mut index = 0;
    while index < words.len() && is_assignment_word(words[index]) {
        index += 1;
    }
    if words.get(index).copied() != Some("sudo") {
        return None;
    }
    index += 1;
    const OTHER_VALUE_OPTIONS: &[&str] = &[
        "-p",
        "--prompt",
        "-h",
        "--host",
        "-C",
        "--close-from",
        "-T",
        "--command-timeout",
        "-R",
        "--chroot",
        "-D",
        "--chdir",
    ];
    while index < words.len() {
        let word = words[index];
        let kind = match word {
            "-u" | "--user" => Some(WrapperValueKind::User),
            "-g" | "--group" => Some(WrapperValueKind::Group),
            _ => None,
        };
        if let Some(kind) = kind {
            if index + 1 == words.len() {
                return Some(kind);
            }
            index += 2;
            continue;
        }
        if !word.starts_with('-') {
            return None;
        }
        index += 1;
        if OTHER_VALUE_OPTIONS.contains(&word) {
            index += 1;
        }
    }
    None
}

pub(super) fn extract_word_at(buf: &str) -> (String, usize) {
    let mut start = 0;
    let mut quote = None;
    let mut escaped = false;
    for (index, ch) in buf.char_indices() {
        if escaped {
            escaped = false;
            continue;
        }
        match quote {
            Some('\'') => {
                if ch == '\'' {
                    quote = None;
                }
            }
            Some('"') => match ch {
                '"' => quote = None,
                '\\' => escaped = true,
                _ => {}
            },
            _ => match ch {
                '\\' => escaped = true,
                '\'' | '"' => quote = Some(ch),
                ' ' | '\t' | '|' | '&' | ';' | '(' | ')' | '<' | '>' => {
                    start = index + ch.len_utf8();
                }
                _ => {}
            },
        }
    }
    let word = buf[start..].to_string();
    (word, start)
}

pub(super) fn is_command_position(buf: &str, word_start: usize) -> bool {
    let before = buf[..word_start].trim_end_matches([' ', '\t']);
    if before.is_empty()
        || before.ends_with('|')
        || before.ends_with("&&")
        || before.ends_with("||")
        || before.ends_with(';')
        || before.ends_with('\n')
        || before.ends_with('(')
        || before.ends_with('{')
    {
        return true;
    }

    let words: Vec<&str> = active_command_segment(&buf[..word_start])
        .split_whitespace()
        .collect();
    effective_command_index(&words) >= words.len()
}

pub(super) fn unescape_shell_word(word: &str) -> String {
    let mut result = String::with_capacity(word.len());
    let mut quote = None;
    let mut chars = word.chars().peekable();
    while let Some(ch) = chars.next() {
        match quote {
            Some('\'') => {
                if ch == '\'' {
                    quote = None;
                } else {
                    result.push(ch);
                }
            }
            Some('"') => match ch {
                '"' => quote = None,
                '\\' => {
                    if let Some(next) = chars.next() {
                        result.push(next);
                    }
                }
                _ => result.push(ch),
            },
            _ => match ch {
                '\'' | '"' => quote = Some(ch),
                '\\' => {
                    if let Some(next) = chars.next() {
                        result.push(next);
                    }
                }
                _ => result.push(ch),
            },
        }
    }
    result
}

pub(super) fn escape_shell_word(word: &str) -> String {
    let mut result = String::with_capacity(word.len());
    for ch in word.chars() {
        if ch.is_alphanumeric() || matches!(ch, '/' | '_' | '-' | '.' | '~') {
            result.push(ch);
        } else {
            result.push('\\');
            result.push(ch);
        }
    }
    result
}

/// Split a history line into simple-command segments at unquoted connectors,
/// redirections, and subshell boundaries. Segments that are redirection
/// targets simply fail the head-command comparison later.
pub(super) fn split_command_segments(line: &str) -> Vec<&str> {
    let mut segments = Vec::new();
    let mut start = 0;
    let mut quote = None;
    let mut escaped = false;
    for (index, ch) in line.char_indices() {
        if escaped {
            escaped = false;
            continue;
        }
        match quote {
            Some('\'') => {
                if ch == '\'' {
                    quote = None;
                }
            }
            Some('"') => match ch {
                '"' => quote = None,
                '\\' => escaped = true,
                _ => {}
            },
            _ => match ch {
                '\\' => escaped = true,
                '\'' | '"' => quote = Some(ch),
                '|' | ';' | '&' | '\n' | '<' | '>' | '(' | ')' => {
                    segments.push(&line[start..index]);
                    start = index + ch.len_utf8();
                }
                _ => {}
            },
        }
    }
    segments.push(&line[start..]);
    segments.retain(|segment| !segment.trim().is_empty());
    segments
}

/// Split a command segment into words at unquoted whitespace, keeping each
/// word's original spelling (quotes and escapes included) so it can be
/// inserted back into a command line as typed.
pub(super) fn quote_aware_words(segment: &str) -> Vec<&str> {
    let mut words = Vec::new();
    let mut start = None;
    let mut quote = None;
    let mut escaped = false;
    for (index, ch) in segment.char_indices() {
        if escaped {
            escaped = false;
            continue;
        }
        match quote {
            Some('\'') => {
                if ch == '\'' {
                    quote = None;
                }
            }
            Some('"') => match ch {
                '"' => quote = None,
                '\\' => escaped = true,
                _ => {}
            },
            _ => match ch {
                '\\' => escaped = true,
                '\'' | '"' => {
                    quote = Some(ch);
                    start.get_or_insert(index);
                }
                ch if ch.is_whitespace() => {
                    if let Some(word_start) = start.take() {
                        words.push(&segment[word_start..index]);
                    }
                }
                _ => {
                    start.get_or_insert(index);
                }
            },
        }
    }
    if let Some(word_start) = start {
        words.push(&segment[word_start..]);
    }
    words
}
