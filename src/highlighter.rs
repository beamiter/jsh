/// Real-time syntax highlighting via lenient tokenization.
use crate::environment::ShellState;
use crate::parser::lexer::{self, Token};
use crossterm::style::Color;

pub struct StyledSpan {
    pub text: String,
    pub fg: Option<Color>,
    pub bold: bool,
    pub underline: bool,
}

/// Highlight the input buffer, returning styled spans.
pub fn highlight(buffer: &str, state: &mut ShellState) -> Vec<StyledSpan> {
    let tokens = lexer::tokenize_lenient(buffer);
    let mut spans = Vec::new();
    let mut is_command_pos = true;
    let mut last_end = 0;

    for spanned in &tokens {
        if spanned.token == Token::Eof {
            break;
        }

        let start = spanned.span.0;
        let end = spanned.span.1;

        // Add any gap between tokens as unstyled text
        if start > last_end {
            spans.push(StyledSpan {
                text: buffer[last_end..start].to_string(),
                fg: None,
                bold: false,
                underline: false,
            });
        }

        let text = buffer[start..end].to_string();

        let span = match &spanned.token {
            Token::Word(w) if is_command_pos => {
                let raw = strip_quotes(w);
                // Phase 14d: signed value-aware builtins (try/where/each/etc.)
                // are valid commands even though they aren't in the bash-style
                // BUILTIN_NAMES list, and aren't on PATH. Highlight them
                // identically to other builtins so the prompt no longer
                // marks `try`, `each`, `error` etc. red.
                let is_signed = crate::signature::SIGNATURES.contains_key(raw.as_str());
                let is_user_fn = state.user_signatures.contains_key(&raw)
                    || state.user_typed_fns.contains_key(&raw);
                if is_builtin_cmd(&raw)
                    || is_signed
                    || is_user_fn
                    || state.command_in_path(&raw)
                    || state.aliases.contains_key(&raw)
                    || state.functions.contains_key(&raw)
                {
                    StyledSpan {
                        text,
                        fg: Some(Color::Green),
                        bold: true,
                        underline: false,
                    }
                } else if raw.contains('/') {
                    // Looks like a path — show as valid (skip stat on every keystroke)
                    StyledSpan {
                        text,
                        fg: Some(Color::Green),
                        bold: true,
                        underline: true,
                    }
                } else {
                    StyledSpan {
                        text,
                        fg: Some(Color::Red),
                        bold: true,
                        underline: false,
                    }
                }
            }
            Token::Word(w) => {
                let raw = strip_quotes(w);
                if raw.starts_with('$') {
                    StyledSpan {
                        text,
                        fg: Some(Color::Cyan),
                        bold: false,
                        underline: false,
                    }
                } else if raw.starts_with('-') {
                    StyledSpan {
                        text,
                        fg: Some(Color::White),
                        bold: false,
                        underline: false,
                    }
                } else if raw.starts_with('\'') || raw.starts_with('"') {
                    StyledSpan {
                        text,
                        fg: Some(Color::Yellow),
                        bold: false,
                        underline: false,
                    }
                } else {
                    StyledSpan {
                        text,
                        fg: None,
                        bold: false,
                        underline: false,
                    }
                }
            }
            Token::Pipe | Token::PipeAnd | Token::And | Token::Or => {
                is_command_pos = true;
                StyledSpan {
                    text,
                    fg: Some(Color::Magenta),
                    bold: true,
                    underline: false,
                }
            }
            Token::AmpBang => {
                is_command_pos = true;
                StyledSpan {
                    text,
                    fg: Some(Color::Magenta),
                    bold: true,
                    underline: false,
                }
            }
            Token::Semi | Token::Amp => {
                is_command_pos = true;
                StyledSpan {
                    text,
                    fg: Some(Color::Magenta),
                    bold: false,
                    underline: false,
                }
            }
            Token::RedirectOut
            | Token::RedirectAppend
            | Token::RedirectIn
            | Token::HereDoc
            | Token::HereDocStrip
            | Token::HereString
            | Token::DupFd
            | Token::RedirectFd(_, _) => StyledSpan {
                text,
                fg: Some(Color::Blue),
                bold: false,
                underline: false,
            },
            Token::LParen | Token::RParen | Token::LBrace | Token::RBrace => StyledSpan {
                text,
                fg: Some(Color::Yellow),
                bold: true,
                underline: false,
            },
            _ => StyledSpan {
                text,
                fg: None,
                bold: false,
                underline: false,
            },
        };

        // Track command position
        if matches!(&spanned.token, Token::Word(_)) && is_command_pos {
            is_command_pos = false;
        }

        spans.push(span);
        last_end = end;
    }

    // Remainder
    if last_end < buffer.len() {
        spans.push(StyledSpan {
            text: buffer[last_end..].to_string(),
            fg: None,
            bold: false,
            underline: false,
        });
    }

    // Syntax error detection: mark errors with red underline
    // Only check if the input appears "complete" (not just typing a pipe, etc.)
    let trailing = buffer.trim_end();
    let looks_complete = !trailing.is_empty()
        && !trailing.ends_with('|')
        && !trailing.ends_with("&&")
        && !trailing.ends_with("||")
        && !trailing.ends_with('\\');

    if looks_complete
        && !crate::parser::is_incomplete(buffer)
        && crate::parser::parse(buffer).is_err()
    {
        // Mark the last token/span as error
        if let Some(last_span) = spans.last_mut() {
            last_span.fg = Some(Color::Red);
            last_span.underline = true;
        }
    }

    spans
}

fn is_builtin_cmd(name: &str) -> bool {
    crate::builtins::is_builtin(name)
}

fn strip_quotes(s: &str) -> String {
    let mut result = String::new();
    let mut chars = s.chars().peekable();
    while let Some(c) = chars.next() {
        match c {
            '\'' => {
                while let Some(&c2) = chars.peek() {
                    if c2 == '\'' {
                        chars.next();
                        break;
                    }
                    result.push(c2);
                    chars.next();
                }
            }
            '"' => {
                while let Some(&c2) = chars.peek() {
                    if c2 == '"' {
                        chars.next();
                        break;
                    }
                    result.push(c2);
                    chars.next();
                }
            }
            '\\' => {
                if let Some(c2) = chars.next() {
                    result.push(c2);
                }
            }
            _ => result.push(c),
        }
    }
    result
}
