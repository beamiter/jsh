/// Recursive descent parser for bash-compatible shell grammar.
///
/// Grammar (simplified):
///   program         = complete_command*
///   complete_command = and_or_list ((';' | '&' | '&!') and_or_list)* [';' | '&' | '&!']
///   and_or_list     = pipeline (('&&' | '||') pipeline)*
///   pipeline        = ['!'] command ('|' command)*
///   command         = simple_command | compound_command | function_def
///   simple_command  = (assignment)* word+ (redirect)*
use super::ast::*;
use super::lexer::{Lexer, RedirectOp, SpannedToken, Token};

#[derive(Debug)]
pub enum ParseError {
    Unexpected(String),
    Incomplete,
}

impl std::fmt::Display for ParseError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            ParseError::Unexpected(msg) => write!(f, "syntax error: {}", msg),
            ParseError::Incomplete => write!(f, "incomplete input"),
        }
    }
}

fn copy_balanced(chars: &mut std::iter::Peekable<std::str::Chars>, out: &mut String, close: char) {
    let open = if close == ')' { '(' } else { '{' };
    let mut depth = 0usize;
    for c in chars.by_ref() {
        out.push(c);
        if c == open {
            depth += 1;
        } else if c == close {
            depth -= 1;
            if depth == 0 {
                return;
            }
        }
    }
}

/// Protect the regex metacharacters that word expansion would otherwise claim.
///
/// The `=~` operand still goes through expansion (bash expands `$re` there), but
/// it must not go through pathname or brace expansion: `[0-9]` would glob
/// against the cwd, `a{1,2}` would split into two words, and `\.` would lose its
/// backslash. Quoted stretches and `$(...)`/`${...}`/backticks are copied
/// verbatim, so they keep their own rules.
fn protect_regex_operand(raw: &str) -> String {
    let mut out = String::with_capacity(raw.len() + 8);
    let mut chars = raw.chars().peekable();
    while let Some(c) = chars.next() {
        match c {
            '\'' => {
                out.push(c);
                for c2 in chars.by_ref() {
                    out.push(c2);
                    if c2 == '\'' {
                        break;
                    }
                }
            }
            '"' => {
                out.push(c);
                while let Some(c2) = chars.next() {
                    out.push(c2);
                    if c2 == '\\' {
                        if let Some(c3) = chars.next() {
                            out.push(c3);
                        }
                        continue;
                    }
                    if c2 == '"' {
                        break;
                    }
                }
            }
            '`' => {
                out.push(c);
                for c2 in chars.by_ref() {
                    out.push(c2);
                    if c2 == '`' {
                        break;
                    }
                }
            }
            '$' => {
                out.push(c);
                match chars.peek() {
                    Some(&'(') => copy_balanced(&mut chars, &mut out, ')'),
                    Some(&'{') => copy_balanced(&mut chars, &mut out, '}'),
                    _ => {}
                }
            }
            '\\' | '*' | '?' | '[' | ']' | '{' | '}' | '~' | '<' | '>' => {
                out.push('\\');
                out.push(c);
            }
            _ => out.push(c),
        }
    }
    out
}

/// Split a C-style for header into its `init ; condition ; update` sections on
/// top-level semicolons (those not nested inside parentheses).
fn split_for_header(s: &str) -> Vec<String> {
    let mut parts = Vec::new();
    let mut cur = String::new();
    let mut depth = 0i32;
    for c in s.chars() {
        match c {
            '(' => {
                depth += 1;
                cur.push(c);
            }
            ')' => {
                depth -= 1;
                cur.push(c);
            }
            ';' if depth == 0 => {
                parts.push(cur.trim().to_string());
                cur.clear();
            }
            _ => cur.push(c),
        }
    }
    parts.push(cur.trim().to_string());
    parts
}

/// How the source text after an opening `((` ends.
#[derive(Debug, PartialEq, Eq)]
enum DoubleParenEnd {
    /// The expression is the first `len` bytes, and `))` follows it.
    Arithmetic(usize),
    /// The inner `(` closes without a `)` right behind it, so bash reads the
    /// `((` as two nested subshells: `((cd /tmp && ls) | wc -l)`.
    Subshells,
    /// The input ends inside the expression.
    Incomplete,
}

/// Find the end of the text after `((` the way bash's `parse_arith_cmd` does:
/// follow parenthesis depth to the `)` that closes the inner `(`, then require
/// a second `)` immediately after it. Quotes, backquotes and backslash escapes
/// hide parentheses; `$(...)` and `$((...))` simply nest.
fn scan_double_paren(src: &str) -> DoubleParenEnd {
    let bytes = src.as_bytes();
    let mut depth = 0usize;
    let mut i = 0;
    while i < bytes.len() {
        match bytes[i] {
            b'\\' => i += 1,
            quote @ (b'\'' | b'`') => match bytes[i + 1..].iter().position(|&b| b == quote) {
                Some(offset) => i += offset + 1,
                None => return DoubleParenEnd::Incomplete,
            },
            b'"' => {
                i += 1;
                while i < bytes.len() && bytes[i] != b'"' {
                    if bytes[i] == b'\\' {
                        i += 1;
                    }
                    i += 1;
                }
                if i >= bytes.len() {
                    return DoubleParenEnd::Incomplete;
                }
            }
            b'(' => depth += 1,
            b')' if depth > 0 => depth -= 1,
            b')' => {
                return if bytes.get(i + 1) == Some(&b')') {
                    DoubleParenEnd::Arithmetic(i)
                } else {
                    DoubleParenEnd::Subshells
                };
            }
            _ => {}
        }
        i += 1;
    }
    DoubleParenEnd::Incomplete
}

/// Split the body of an array literal (`arr=(...)`) into element words. Quoting
/// is honored, so `("a b" c)` is two elements and not three, and `#` starting a
/// word begins a comment as it does in bash.
fn split_array_elements(s: &str) -> Vec<String> {
    let mut elems = Vec::new();
    let mut cur = String::new();
    // Constructs still open, inside which whitespace does not separate
    // elements: `"` for double quotes, `` ` `` for backquotes, `)` for `$(`
    // (and parentheses nested in it), `}` for `${`. Each may contain the
    // others, so `"$(echo "1 2")"` is one element.
    let mut open: Vec<char> = Vec::new();
    let mut chars = s.chars().peekable();

    while let Some(c) = chars.next() {
        let top = open.last().copied();
        match c {
            '\\' => {
                cur.push(c);
                if let Some(c2) = chars.next() {
                    cur.push(c2);
                }
            }
            // Single quotes are literal text inside double quotes.
            '\'' if top != Some('"') => {
                cur.push(c);
                for c2 in chars.by_ref() {
                    cur.push(c2);
                    if c2 == '\'' {
                        break;
                    }
                }
            }
            '"' | '`' => {
                cur.push(c);
                if top == Some(c) {
                    open.pop();
                } else {
                    open.push(c);
                }
            }
            '$' if matches!(chars.peek(), Some(&'(') | Some(&'{')) => {
                cur.push(c);
                let opener = chars.next().expect("peeked");
                cur.push(opener);
                open.push(if opener == '(' { ')' } else { '}' });
            }
            '(' if top == Some(')') => {
                open.push(')');
                cur.push(c);
            }
            ')' | '}' if top == Some(c) => {
                open.pop();
                cur.push(c);
            }
            '#' if open.is_empty() && cur.is_empty() => {
                while let Some(&c2) = chars.peek() {
                    if c2 == '\n' {
                        break;
                    }
                    chars.next();
                }
            }
            ' ' | '\t' | '\n' | '\r' if open.is_empty() => {
                if !cur.is_empty() {
                    elems.push(std::mem::take(&mut cur));
                }
            }
            _ => cur.push(c),
        }
    }

    if !cur.is_empty() {
        elems.push(cur);
    }
    elems
}

pub struct Parser<'a> {
    lexer: Lexer<'a>,
    current: SpannedToken,
    peeked: Option<SpannedToken>,
    input: &'a str,
    // Pending here-doc body regions to skip: (newline_trigger_pos, resume_pos).
    // When the line-ending newline at trigger is consumed, the lexer jumps to
    // resume, stepping over the here-doc body that was already collected.
    heredoc_skips: Vec<(usize, usize)>,
    /// Byte offset of every newline in `input`, collected on first use so a
    /// line lookup is a binary search rather than a rescan of the prefix.
    newlines: Option<Vec<usize>>,
    /// Span of the token `advance` consumed most recently.
    prev_span: (usize, usize),
    /// Tokens consumed while a command's `BASH_COMMAND` text is being
    /// collected: each span, and whether the next token follows it without a
    /// space (`2>&1`, `<<EOF`, `{fd}>`).
    recording: Option<Vec<((usize, usize), bool)>>,
}

impl<'a> Parser<'a> {
    pub fn new(input: &'a str) -> Self {
        let mut lexer = Lexer::new(input);
        let current = lexer.next_token();
        Parser {
            lexer,
            current,
            peeked: None,
            input,
            heredoc_skips: Vec::new(),
            newlines: None,
            prev_span: (0, 0),
            recording: None,
        }
    }

    /// 1-based line of byte `offset`.
    fn line_at(&mut self, offset: usize) -> usize {
        let input = self.input;
        let newlines = self.newlines.get_or_insert_with(|| {
            input
                .bytes()
                .enumerate()
                .filter_map(|(i, b)| (b == b'\n').then_some(i))
                .collect()
        });
        newlines.partition_point(|&nl| nl < offset) + 1
    }

    /// Line holding the last character of a token. A token that spans lines
    /// (a quoted newline, a line continuation before it) reports its last
    /// line, which is where bash's lexer is when it has read the token.
    fn end_line(&mut self, span: (usize, usize)) -> usize {
        self.line_at(span.1.saturating_sub(1).max(span.0))
    }

    fn peek_span(&mut self) -> (usize, usize) {
        self.peek();
        self.peeked
            .as_ref()
            .map_or(self.current.span, |token| token.span)
    }

    /// Start collecting the tokens of a command's text, returning any outer
    /// collection so it can be resumed afterwards.
    fn begin_recording(&mut self) -> Option<Vec<((usize, usize), bool)>> {
        self.recording.replace(Vec::new())
    }

    /// Finish the current collection and render it the way bash prints a
    /// command: tokens as written, separated by one space.
    fn finish_recording(&mut self, outer: Option<Vec<((usize, usize), bool)>>) -> String {
        let tokens = std::mem::replace(&mut self.recording, outer).unwrap_or_default();
        let mut text = String::new();
        let mut glue = true;
        for ((start, end), glue_next) in tokens {
            if !glue {
                text.push(' ');
            }
            text.push_str(self.input.get(start..end).unwrap_or_default());
            glue = glue_next;
        }
        text
    }

    /// Mark the most recently collected token as joined to the next one.
    fn glue_recorded_token(&mut self) {
        if let Some(last) = self.recording.as_mut().and_then(|tokens| tokens.last_mut()) {
            last.1 = true;
        }
    }

    fn advance(&mut self) {
        self.prev_span = self.current.span;
        if let Some(tokens) = self.recording.as_mut() {
            // Line breaks inside a header (`for x\nin ...`) are not part of
            // the command's printed form.
            if self.current.token != Token::Newline {
                let glue = matches!(
                    self.current.token,
                    Token::DupFd
                        | Token::HereDoc
                        | Token::HereDocStrip
                        | Token::RedirectFd(_, RedirectOp::DupOutput | RedirectOp::DupInput)
                );
                tokens.push((self.current.span, glue));
            }
        }
        if let Some(t) = self.peeked.take() {
            self.current = t;
        } else {
            self.current = self.lexer.next_token();
        }
        self.apply_heredoc_skip();
    }

    /// If the current token is a line-ending newline that has a pending here-doc
    /// body after it, jump the lexer past that body.
    fn apply_heredoc_skip(&mut self) {
        if self.current.token != Token::Newline || self.heredoc_skips.is_empty() {
            return;
        }
        let pos = self.current.span.0;
        if let Some(i) = self
            .heredoc_skips
            .iter()
            .position(|(trigger, _)| *trigger == pos)
        {
            let (_, resume) = self.heredoc_skips.remove(i);
            self.lexer.set_pos(resume.min(self.input.len()));
            self.peeked = None;
        }
    }

    fn peek(&mut self) -> &Token {
        if self.peeked.is_none() {
            self.peeked = Some(self.lexer.next_token());
        }
        &self.peeked.as_ref().unwrap().token
    }

    fn expect_word(&mut self) -> Result<String, ParseError> {
        match &self.current.token {
            Token::Word(w) => {
                let w = w.clone();
                self.advance();
                Ok(w)
            }
            Token::Eof => Err(ParseError::Incomplete),
            other => Err(ParseError::Unexpected(format!(
                "expected word, got {:?}",
                other
            ))),
        }
    }

    /// Collect a here-doc body. `after_delim` is the input offset just past the
    /// delimiter word. The body begins at the first newline at/after that offset
    /// (so any same-line tokens like `| sort` are left for normal lexing) and runs
    /// until a line equal to the delimiter. A skip is registered so the lexer steps
    /// over the body once the line-ending newline is consumed.
    fn collect_here_doc_content(
        &mut self,
        delimiter: &str,
        strip_tabs: bool,
        after_delim: usize,
    ) -> Result<String, ParseError> {
        let input = self.input;
        let after_delim = after_delim.min(input.len());

        // Find the newline that ends the line carrying the `<<` operator.
        let nl1 = match input[after_delim..].find('\n') {
            Some(off) => after_delim + off,
            None => return Err(ParseError::Incomplete),
        };
        let body_start = nl1 + 1;

        let remaining = &input[body_start..];
        let mut content = String::new();
        let mut line_start = 0usize;
        let mut resume = input.len();
        let mut found_delimiter = false;

        for segment in remaining.split_inclusive('\n') {
            let line = segment.strip_suffix('\n').unwrap_or(segment);
            let line_len = segment.len();
            let trimmed = if strip_tabs {
                line.trim_start_matches('\t')
            } else {
                line
            };

            if trimmed == delimiter {
                resume = (body_start + line_start + line_len).min(input.len());
                found_delimiter = true;
                break;
            }

            content.push_str(trimmed);
            if segment.ends_with('\n') {
                content.push('\n');
            }
            line_start += line_len;
        }

        if !found_delimiter {
            return Err(ParseError::Incomplete);
        }

        // Defer the body skip until the line-ending newline at nl1 is consumed.
        self.heredoc_skips.push((nl1, resume));
        // If that newline is the current (or buffered) token, the skip must fire now.
        self.apply_heredoc_skip();

        Ok(content)
    }

    fn skip_newlines(&mut self) {
        while self.current.token == Token::Newline {
            self.advance();
        }
    }

    fn is_keyword(&self, kw: &str) -> bool {
        matches!(&self.current.token, Token::Word(w) if w == kw)
    }

    fn expect_keyword(&mut self, kw: &str) -> Result<(), ParseError> {
        if self.is_keyword(kw) {
            self.advance();
            Ok(())
        } else if self.current.token == Token::Eof {
            Err(ParseError::Incomplete)
        } else {
            Err(ParseError::Unexpected(format!(
                "expected '{}', got {:?}",
                kw, self.current.token
            )))
        }
    }

    fn is_redirect(&self) -> bool {
        token_is_redirect(&self.current.token)
    }

    /// `{name}` immediately followed by a redirection operator: the shell
    /// allocates the descriptor and stores its number in `name`.
    fn fd_variable_prefix(&mut self) -> Option<String> {
        let Token::Word(word) = &self.current.token else {
            return None;
        };
        let name = word.strip_prefix('{')?.strip_suffix('}')?;
        if !is_fd_variable_name(name) {
            return None;
        }
        let name = name.to_string();
        let end = self.current.span.1;
        let next = self.peek_span();
        let adjacent = next.0 == end;
        // `{x}2>file` is a word followed by an ordinary redirection.
        let numbered = self
            .input
            .as_bytes()
            .get(next.0)
            .is_some_and(u8::is_ascii_digit);
        let redirect = !numbered
            && self.peeked.as_ref().is_some_and(|t| {
                token_is_redirect(&t.token)
                    && !matches!(
                        t.token,
                        // Here-documents and `&>` have no single descriptor.
                        Token::HereDoc
                            | Token::HereDocStrip
                            | Token::HereString
                            | Token::RedirectAllOut
                            | Token::RedirectAllAppend
                    )
            });
        (adjacent && redirect).then_some(name)
    }

    fn parse_redirect(&mut self) -> Result<Redirect, ParseError> {
        let is_heredoc_strip = self.current.token == Token::HereDocStrip;
        let (fd, kind, is_here_doc) = match &self.current.token {
            Token::RedirectOut => (None, RedirectKind::Output, false),
            Token::RedirectClobber => (None, RedirectKind::Clobber, false),
            Token::RedirectAppend => (None, RedirectKind::Append, false),
            Token::RedirectIn => (None, RedirectKind::Input, false),
            Token::HereString => (None, RedirectKind::HereString, true),
            Token::HereDoc | Token::HereDocStrip => (None, RedirectKind::HereDoc, true),
            Token::DupFd => (None, RedirectKind::DupOutput, false),
            Token::RedirectAllOut => (None, RedirectKind::OutputAll, false),
            Token::RedirectAllAppend => (None, RedirectKind::AppendAll, false),
            Token::RedirectFd(n, op) => {
                let fd = Some(*n);
                let kind = match op {
                    RedirectOp::Output => RedirectKind::Output,
                    RedirectOp::Clobber => RedirectKind::Clobber,
                    RedirectOp::Append => RedirectKind::Append,
                    RedirectOp::Input => RedirectKind::Input,
                    RedirectOp::DupOutput => RedirectKind::DupOutput,
                    RedirectOp::DupInput => RedirectKind::DupInput,
                };
                (fd, kind, false)
            }
            _ => return Err(ParseError::Unexpected("expected redirect".into())),
        };
        self.advance();
        // Anchor here-doc body collection to the end of the delimiter token, before
        // expect_word advances the lexer (and any look-ahead) past it.
        let delim_span_end = self.current.span.1;
        let target_str = self.expect_word()?;

        // For here-doc and here-string, collect the content
        let here_doc_opt = if is_here_doc {
            let delimiter = target_str.clone();

            let (content, expand_vars) = if kind == RedirectKind::HereDoc {
                // A quoted or backslash-escaped delimiter suppresses expansion.
                let quoted = delimiter.starts_with('\\')
                    || delimiter.starts_with('\'')
                    || delimiter.starts_with('"');
                let clean_delim = delimiter
                    .trim_matches(|c| c == '\\' || c == '\'' || c == '"')
                    .to_string();
                let c =
                    self.collect_here_doc_content(&clean_delim, is_heredoc_strip, delim_span_end)?;
                (c, !quoted)
            } else {
                // HereString: content is the target string itself; expansion happens
                // later via the word-part stage (which respects its own quoting).
                (format!("{}\n", delimiter), true)
            };

            Some(HereDocOptions {
                delimiter,
                content,
                strip_tabs: is_heredoc_strip,
                expand_vars,
            })
        } else {
            None
        };

        let target = parse_word_parts(&target_str);
        Ok(Redirect {
            fd,
            kind,
            target,
            here_doc: here_doc_opt,
            fd_var: None,
        })
    }

    fn is_command_start(&mut self) -> bool {
        let is_do = matches!(&self.current.token, Token::Word(w) if w == "do");
        if is_do {
            let is_closure = matches!(self.peek(), Token::Word(nw) if nw.starts_with("{|"));
            if is_closure {
                return true;
            }
        }
        match &self.current.token {
            Token::Word(w) => !is_list_terminator(w),
            Token::LParen | Token::LBrace => true,
            _ => self.is_redirect(),
        }
    }

    fn parse_simple_command(&mut self) -> Result<Command, ParseError> {
        let mut assignments = Vec::new();
        let mut words: Vec<Word> = Vec::new();
        let mut redirects = Vec::new();

        // Bash records a simple command's line when its grammar reduces the
        // first element. After a plain word that needs one token of look-ahead
        // (the word could still start `name()`), so `echo "a<newline>b"`
        // reports the line the string ends on; an assignment or redirection
        // is reduced on its own last token.
        let first_is_word = matches!(&self.current.token, Token::Word(w) if !is_assignment(w));
        let mut line = if first_is_word {
            let span = self.peek_span();
            self.end_line(span)
        } else {
            0
        };
        let outer_recording = self.begin_recording();

        // Parse leading assignments (VAR=value, arr[idx]=value, arr=(a b c), var+=value)
        loop {
            let is_assign = if let Token::Word(w) = &self.current.token {
                words.is_empty() && is_assignment(w)
            } else {
                false
            };
            if is_assign {
                let w = if let Token::Word(w) = &self.current.token {
                    w.clone()
                } else {
                    unreachable!()
                };
                let assign = match parse_assignment(&w, self) {
                    Ok(assign) => assign,
                    Err(error) => {
                        self.finish_recording(outer_recording);
                        return Err(error);
                    }
                };
                assignments.push(assign);
                if line == 0 {
                    line = self.end_line(self.prev_span);
                }
                continue;
            }
            break;
        }

        let mut compound_args = Vec::new();
        let result = self.parse_simple_command_rest(
            &mut words,
            &mut redirects,
            &mut compound_args,
            &mut line,
        );
        let text = self.finish_recording(outer_recording);
        result?;

        if words.is_empty() && assignments.is_empty() && redirects.is_empty() {
            if self.current.token == Token::Eof {
                return Err(ParseError::Incomplete);
            }
            return Err(ParseError::Unexpected(format!(
                "expected command, got {:?}",
                self.current.token
            )));
        }

        Ok(Command::Simple(SimpleCommand {
            assignments,
            words,
            redirects,
            line,
            text,
            compound_args,
        }))
    }

    /// The words and redirections of a simple command, after its assignments.
    fn parse_simple_command_rest(
        &mut self,
        words: &mut Vec<Word>,
        redirects: &mut Vec<Redirect>,
        compound_args: &mut Vec<(usize, String)>,
        line: &mut usize,
    ) -> Result<(), ParseError> {
        loop {
            if let Some(name) = self.fd_variable_prefix() {
                self.advance();
                self.glue_recorded_token();
                let mut redirect = self.parse_redirect()?;
                redirect.fd_var = Some(name);
                redirects.push(redirect);
            } else if self.is_redirect() {
                redirects.push(self.parse_redirect()?);
                if *line == 0 {
                    *line = self.end_line(self.prev_span);
                }
            } else if let Token::Word(w) = &self.current.token {
                let w = w.clone();
                if is_declaration_command(words) && split_compound_assignment(&w).is_some() {
                    compound_args.push((words.len(), w.clone()));
                }
                words.push(parse_word_parts(&w));
                self.advance();
                // `[[` opens a conditional expression: everything up to the
                // matching `]]` belongs to it, including operator characters.
                if words.len() == 1 && w == "[[" {
                    self.parse_conditional_words(words)?;
                }
            } else {
                return Ok(());
            }
        }
    }

    /// Collect the body of a `[[ ... ]]` conditional, stopping after the closing
    /// `]]`. Inside a conditional, `&&`, `||`, `(`, `)`, `<` and `>` are part of
    /// the expression rather than list operators or redirections, so the
    /// operator tokens the lexer produced are turned back into plain words for
    /// the `[[` builtin to evaluate.
    fn parse_conditional_words(&mut self, words: &mut Vec<Word>) -> Result<(), ParseError> {
        loop {
            let op = match &self.current.token {
                Token::Word(w) => {
                    let w = w.clone();
                    words.push(parse_word_parts(&w));
                    self.advance();
                    if w == "]]" {
                        return Ok(());
                    }
                    // The right operand of `=~` is a regex, not an expression:
                    // it has to survive as ONE word. `(a|z)bc` reaches us as
                    // LParen/Word/Pipe/Word/RParen/Word, and pushing those
                    // separately handed `Regex::new("(")` a broken pattern (and
                    // `|` fell out of the conditional entirely).
                    if w == "=~" {
                        self.parse_regex_operand(words)?;
                    }
                    continue;
                }
                Token::And => "&&",
                Token::Or => "||",
                Token::LParen => "(",
                Token::RParen => ")",
                Token::RedirectIn => "<",
                Token::RedirectOut => ">",
                // A conditional may be continued across lines until its `]]`.
                Token::Newline => {
                    self.advance();
                    continue;
                }
                Token::Eof => return Err(ParseError::Incomplete),
                // Anything else cannot appear in a conditional; hand it back so
                // the normal parse path reports it.
                _ => return Ok(()),
            };
            words.push(vec![WordPart::Literal(op.to_string())]);
            self.advance();
        }
    }

    /// Take the right operand of `[[ ... =~ ... ]]` as a single word, straight
    /// from the source text: the lexer's token boundaries are meaningless inside
    /// a regex, so adjacent tokens (no whitespace between their spans) are one
    /// operand.
    fn parse_regex_operand(&mut self, words: &mut Vec<Word>) -> Result<(), ParseError> {
        match &self.current.token {
            Token::Eof => return Err(ParseError::Incomplete),
            // `[[ x =~ ]]` is malformed; let the normal path report it.
            Token::Newline => return Ok(()),
            Token::Word(w) if w == "]]" => return Ok(()),
            _ => {}
        }
        let start = self.current.span.0;
        let mut end = self.current.span.1;
        self.advance();
        loop {
            match &self.current.token {
                Token::Eof | Token::Newline => break,
                // A gap between spans is whitespace: the operand ended.
                _ if self.current.span.0 != end => break,
                _ => {
                    end = self.current.span.1;
                    self.advance();
                }
            }
        }
        let raw = &self.input[start..end];
        // Quoted parts survive as quoted word parts; the conditional expander
        // makes their text literal within the regex.
        words.push(parse_word_parts(&protect_regex_operand(raw)));
        Ok(())
    }

    fn parse_compound_command(&mut self) -> Result<Command, ParseError> {
        match &self.current.token {
            Token::LParen => {
                // Only touching parentheses open an arithmetic command, as in
                // bash: `( (cmd) )` is two subshells.
                if self.at_double_paren() {
                    self.parse_arithmetic_command()
                } else {
                    self.parse_subshell()
                }
            }
            Token::LBrace => self.parse_brace_group(),
            Token::Word(w) => {
                match w.as_str() {
                    "if" => self.parse_if(),
                    "for" => self.parse_for(),
                    "while" => self.parse_while(),
                    "until" => self.parse_until(),
                    "case" => self.parse_case(),
                    "select" => {
                        // Phase 5a: `select` is also a value-aware projection
                        // builtin (nushell-style). Only treat it as the bash
                        // compound `select var in ...; do ...; done` when the
                        // very next token is a Word that doesn't look like a
                        // pipe argument and is followed by `in`/`do`.
                        if self.looks_like_select_compound() {
                            self.parse_select()
                        } else {
                            self.parse_simple_command()
                        }
                    }
                    _ => self.parse_simple_command(),
                }
            }
            _ => self.parse_simple_command(),
        }
    }

    fn parse_subshell(&mut self) -> Result<Command, ParseError> {
        self.advance(); // consume (
        self.skip_newlines();
        let body = self.parse_command_list()?;
        if self.current.token != Token::RParen {
            return Err(if self.current.token == Token::Eof {
                ParseError::Incomplete
            } else {
                ParseError::Unexpected(format!("expected ')', got {:?}", self.current.token))
            });
        }
        self.advance();
        let redirects = self.parse_optional_redirects()?;
        Ok(Command::Compound(CompoundCommand::Subshell {
            body,
            redirects,
        }))
    }

    fn parse_brace_group(&mut self) -> Result<Command, ParseError> {
        self.advance(); // consume {
        self.skip_newlines();
        let body = self.parse_command_list()?;
        if self.current.token != Token::RBrace {
            return Err(if self.current.token == Token::Eof {
                ParseError::Incomplete
            } else {
                ParseError::Unexpected(format!("expected '}}', got {:?}", self.current.token))
            });
        }
        self.advance();
        let redirects = self.parse_optional_redirects()?;
        Ok(Command::Compound(CompoundCommand::BraceGroup {
            body,
            redirects,
        }))
    }

    fn parse_if(&mut self) -> Result<Command, ParseError> {
        self.advance(); // consume "if"
        self.skip_newlines();
        let mut conditions = Vec::new();
        let condition = self.parse_command_list()?;
        self.expect_keyword("then")?;
        self.skip_newlines();
        let body = self.parse_command_list()?;
        conditions.push((condition, body));

        let mut else_branch = None;
        loop {
            if self.is_keyword("elif") {
                self.advance();
                self.skip_newlines();
                let cond = self.parse_command_list()?;
                self.expect_keyword("then")?;
                self.skip_newlines();
                let body = self.parse_command_list()?;
                conditions.push((cond, body));
            } else if self.is_keyword("else") {
                self.advance();
                self.skip_newlines();
                else_branch = Some(self.parse_command_list()?);
                break;
            } else {
                break;
            }
        }
        self.expect_keyword("fi")?;
        let redirects = self.parse_optional_redirects()?;
        Ok(Command::Compound(CompoundCommand::If {
            conditions,
            else_branch,
            redirects,
        }))
    }

    fn parse_for(&mut self) -> Result<Command, ParseError> {
        let line = self.end_line(self.current.span);
        let outer_recording = self.begin_recording();
        self.advance(); // consume "for"

        // Check if this is C-style for: for ((
        if let Token::LParen = self.current.token {
            if let Token::LParen = self.peek() {
                self.finish_recording(outer_recording);
                return self.parse_c_style_for();
            }
        }

        let header = self.parse_loop_header();
        let text = self.finish_recording(outer_recording);
        let (var, words) = header?;

        self.skip_newlines();
        self.expect_keyword("do")?;
        self.skip_newlines();
        let body = self.parse_command_list()?;
        self.expect_keyword("done")?;
        let redirects = self.parse_optional_redirects()?;
        Ok(Command::Compound(CompoundCommand::For {
            var,
            words,
            body,
            redirects,
            line,
            text,
        }))
    }

    /// `NAME [in WORD...]` of a `for` or `select`, stopping before the
    /// separator so the header text excludes it; the separator is consumed
    /// afterwards.
    fn parse_loop_header(&mut self) -> Result<(String, Option<Vec<Word>>), ParseError> {
        let var = self.expect_word()?;
        self.skip_newlines();

        let words = if self.is_keyword("in") {
            self.advance();
            let mut ws = Vec::new();
            while let Token::Word(w) = &self.current.token {
                let w = w.clone();
                ws.push(parse_word_parts(&w));
                self.advance();
            }
            Some(ws)
        } else {
            None
        };
        let outer = self.recording.take();
        // consume separator
        if self.current.token == Token::Semi || self.current.token == Token::Newline {
            self.advance();
        }
        self.recording = outer;
        Ok((var, words))
    }

    fn parse_c_style_for(&mut self) -> Result<Command, ParseError> {
        // The header is read from the source text, so `;;` in `for ((;;))` and
        // operators such as `<=` never pass through the shell lexer.
        let Some(raw) = self.read_double_paren_body()? else {
            return Err(ParseError::Unexpected(
                "expected `))' to close the arithmetic for header".into(),
            ));
        };
        let parts = split_for_header(&raw);
        let init = parts.first().cloned().unwrap_or_default();
        let condition = parts.get(1).cloned().unwrap_or_default();
        let update = parts.get(2).cloned().unwrap_or_default();

        // Optional separator (`;` or newline) between )) and `do`.
        if self.current.token == Token::Semi {
            self.advance();
        }
        self.skip_newlines();
        self.expect_keyword("do")?;
        self.skip_newlines();
        let body = self.parse_command_list()?;
        self.expect_keyword("done")?;
        let redirects = self.parse_optional_redirects()?;

        Ok(Command::Compound(CompoundCommand::CStyleFor {
            init: init.trim().to_string(),
            condition: condition.trim().to_string(),
            update: update.trim().to_string(),
            body,
            redirects,
        }))
    }

    fn parse_while(&mut self) -> Result<Command, ParseError> {
        self.advance(); // consume "while"
        self.skip_newlines();
        let condition = self.parse_command_list()?;
        self.expect_keyword("do")?;
        self.skip_newlines();
        let body = self.parse_command_list()?;
        self.expect_keyword("done")?;
        let redirects = self.parse_optional_redirects()?;
        Ok(Command::Compound(CompoundCommand::While {
            condition,
            body,
            redirects,
        }))
    }

    fn parse_until(&mut self) -> Result<Command, ParseError> {
        self.advance(); // consume "until"
        self.skip_newlines();
        let condition = self.parse_command_list()?;
        self.expect_keyword("do")?;
        self.skip_newlines();
        let body = self.parse_command_list()?;
        self.expect_keyword("done")?;
        let redirects = self.parse_optional_redirects()?;
        Ok(Command::Compound(CompoundCommand::Until {
            condition,
            body,
            redirects,
        }))
    }

    fn parse_case(&mut self) -> Result<Command, ParseError> {
        let line = self.end_line(self.current.span);
        let outer_recording = self.begin_recording();
        let header = (|| {
            self.advance(); // consume "case"
            let word_str = self.expect_word()?;
            self.skip_newlines();
            self.expect_keyword("in")?;
            Ok(word_str)
        })();
        // Bash prints the header with the space that would separate it from
        // the first clause.
        let text = self.finish_recording(outer_recording) + " ";
        let word = parse_word_parts(&header?);
        self.skip_newlines();

        let mut arms = Vec::new();
        while !self.is_keyword("esac") && self.current.token != Token::Eof {
            // optional (
            if self.current.token == Token::LParen {
                self.advance();
            }
            let mut patterns = Vec::new();
            let p = self.expect_word()?;
            patterns.push(parse_word_parts(&p));
            while let Token::Pipe = &self.current.token {
                self.advance();
                let p = self.expect_word()?;
                patterns.push(parse_word_parts(&p));
            }
            if self.current.token != Token::RParen {
                return Err(ParseError::Unexpected(format!(
                    "expected ')' in case, got {:?}",
                    self.current.token
                )));
            }
            self.advance();
            self.skip_newlines();
            let body = self.parse_command_list()?;
            let terminator = match self.current.token {
                Token::DoubleSemi => {
                    self.advance();
                    CaseTerminator::Break
                }
                Token::SemiAmp => {
                    self.advance();
                    CaseTerminator::FallThrough
                }
                Token::DoubleSemiAmp => {
                    self.advance();
                    CaseTerminator::ContinueMatch
                }
                _ => CaseTerminator::Break, // last arm: directly before esac
            };
            self.skip_newlines();
            arms.push(CaseArm {
                patterns,
                body,
                terminator,
            });
        }
        self.expect_keyword("esac")?;
        let redirects = self.parse_optional_redirects()?;
        Ok(Command::Compound(CompoundCommand::Case {
            word,
            arms,
            redirects,
            line,
            text,
        }))
    }

    /// Heuristic: is the current `select` token followed by `var in ...; do`?
    /// If not, the user means our value-aware projection builtin.
    fn looks_like_select_compound(&mut self) -> bool {
        // We're sitting on Word("select"). Look at the next two tokens by
        // saving lexer state, then restoring.
        let pos_before = self.lexer.pos();
        let cur_save = self.current.clone();
        let peek_save = self.peeked.clone();
        let mut found = false;
        // peek the token after `select`
        let next = self.peek().clone();
        if let Token::Word(w) = &next {
            // bash select var must be a plain identifier (no flags, no dots).
            if !w.is_empty()
                && w.chars()
                    .next()
                    .map(|c| c.is_alphabetic() || c == '_')
                    .unwrap_or(false)
                && w.chars().all(|c| c.is_alphanumeric() || c == '_')
            {
                // Now peek one further: tentatively advance past `select` and the var.
                let saved_current = self.current.clone();
                let saved_peeked = self.peeked.clone();
                self.advance(); // consume select
                self.advance(); // consume var
                                // skip newlines / semicolons
                while matches!(self.current.token, Token::Newline | Token::Semi) {
                    self.advance();
                }
                if let Token::Word(kw) = &self.current.token {
                    if kw == "in" || kw == "do" {
                        found = true;
                    }
                }
                // restore
                self.current = saved_current;
                self.peeked = saved_peeked;
                self.lexer.set_pos(pos_before);
            }
        }
        // restore on the negative branches too (peek() doesn't consume, but be safe)
        self.current = cur_save;
        self.peeked = peek_save;
        if !found {
            self.lexer.set_pos(pos_before);
        }
        found
    }

    fn parse_select(&mut self) -> Result<Command, ParseError> {
        let line = self.end_line(self.current.span);
        let outer_recording = self.begin_recording();
        self.advance(); // consume "select"
        let header = self.parse_loop_header();
        let text = self.finish_recording(outer_recording);
        let (var, words) = header?;

        self.skip_newlines();
        self.expect_keyword("do")?;
        self.skip_newlines();
        let body = self.parse_command_list()?;
        self.expect_keyword("done")?;
        let redirects = self.parse_optional_redirects()?;
        Ok(Command::Compound(CompoundCommand::Select {
            var,
            words,
            body,
            redirects,
            line,
            text,
        }))
    }

    fn parse_arithmetic_command(&mut self) -> Result<Command, ParseError> {
        let Some(expr) = self.read_double_paren_body()? else {
            return self.parse_subshell();
        };
        let redirects = self.parse_optional_redirects()?;
        Ok(Command::Compound(CompoundCommand::Arithmetic {
            expr,
            redirects,
        }))
    }

    /// Is the current `(` immediately followed by another `(`?
    fn at_double_paren(&mut self) -> bool {
        if self.current.token != Token::LParen || *self.peek() != Token::LParen {
            return false;
        }
        let end = self.current.span.1;
        self.peeked.as_ref().is_some_and(|next| next.span.0 == end)
    }

    /// With `((` as the current and peeked tokens, take the text up to the
    /// matching `))` verbatim from the source and resume lexing after it. The
    /// shell lexer must never see that text: it would turn `<=`, `>=`, `<<=`
    /// and `>>=` into redirections or a here-doc. `None` (nothing consumed)
    /// means the parentheses close as nested subshells instead.
    fn read_double_paren_body(&mut self) -> Result<Option<String>, ParseError> {
        self.peek();
        let start = self
            .peeked
            .as_ref()
            .map_or(self.current.span.1, |next| next.span.1);
        match scan_double_paren(&self.input[start..]) {
            DoubleParenEnd::Arithmetic(len) => {
                let body = self.input[start..start + len].to_string();
                self.peeked = None;
                self.lexer.set_pos(start + len + 2);
                self.advance();
                Ok(Some(body))
            }
            DoubleParenEnd::Subshells => Ok(None),
            DoubleParenEnd::Incomplete => Err(ParseError::Incomplete),
        }
    }

    fn parse_optional_redirects(&mut self) -> Result<Vec<Redirect>, ParseError> {
        let mut redirects = Vec::new();
        while self.is_redirect() {
            redirects.push(self.parse_redirect()?);
        }
        Ok(redirects)
    }

    fn parse_command(&mut self) -> Result<Command, ParseError> {
        // Check for function definition: name() { ... }
        if let Token::Word(name) = &self.current.token {
            let name = name.clone();
            if !is_reserved_word(&name) {
                if let Token::LParen = self.peek() {
                    // Could be function def
                    let saved = self.current.clone();
                    self.advance(); // consume name
                    if self.current.token == Token::LParen {
                        self.advance(); // consume (
                        if self.current.token == Token::RParen {
                            self.advance(); // consume )
                            self.skip_newlines();
                            let body = self.parse_compound_command()?;
                            if let Command::Compound(c) = body {
                                return Ok(Command::FunctionDef {
                                    name,
                                    body: Box::new(c),
                                });
                            }
                        }
                    }
                    // Restore - not a function def
                    self.current = saved;
                }
            }
        }

        // Check for compound commands
        match &self.current.token {
            Token::LParen | Token::LBrace => self.parse_compound_command(),
            Token::Word(w) if is_compound_keyword(w) => self.parse_compound_command(),
            Token::Word(w) if w == "coproc" => {
                self.advance(); // consume "coproc"
                                // `coproc NAME compound` names the coprocess; a simple command
                                // cannot be named, so its first word is always the command.
                let mut name = None;
                if !self.at_compound_start() {
                    if let Token::Word(word) = &self.current.token {
                        let word = word.clone();
                        let next_is_compound = match self.peek() {
                            Token::LParen | Token::LBrace => true,
                            Token::Word(next) => is_compound_keyword(next),
                            _ => false,
                        };
                        if next_is_compound && is_fd_variable_name(&word) && !word.contains('[') {
                            name = Some(word);
                            self.advance();
                        }
                    }
                }
                let command = if name.is_some() || self.at_compound_start() {
                    self.parse_compound_command()?
                } else {
                    self.parse_simple_command()?
                };
                Ok(Command::Compound(CompoundCommand::Coproc {
                    name,
                    command: Box::new(command),
                    redirects: Vec::new(),
                }))
            }
            Token::Word(w) if w == "function" => {
                self.advance(); // consume "function"
                let name = self.expect_word()?;
                // optional ()
                if self.current.token == Token::LParen {
                    self.advance();
                    if self.current.token == Token::RParen {
                        self.advance();
                    }
                }
                self.skip_newlines();
                let body = self.parse_compound_command()?;
                if let Command::Compound(c) = body {
                    Ok(Command::FunctionDef {
                        name,
                        body: Box::new(c),
                    })
                } else {
                    Err(ParseError::Unexpected(
                        "expected compound command after function name".into(),
                    ))
                }
            }
            _ => self.parse_simple_command(),
        }
    }

    /// True when the current token opens a compound command.
    fn at_compound_start(&self) -> bool {
        match &self.current.token {
            Token::LParen | Token::LBrace => true,
            Token::Word(w) => is_compound_keyword(w),
            _ => false,
        }
    }

    fn parse_pipeline(&mut self) -> Result<Pipeline, ParseError> {
        // `time` and `!` prefix a pipeline in either order.
        let mut negated = false;
        let mut timed = None;
        loop {
            if !negated && self.is_keyword("!") {
                self.advance();
                negated = true;
            } else if timed.is_none() && self.is_keyword("time") {
                self.advance();
                timed = Some(TimeFormat::Shell);
                if self.is_keyword("-p") {
                    self.advance();
                    timed = Some(TimeFormat::Posix);
                }
            } else {
                break;
            }
        }

        // A bare `time` reports the (empty) timing of nothing, as in bash.
        if timed.is_some() && !self.is_command_start() {
            return Ok(Pipeline {
                negated,
                commands: Vec::new(),
                timed,
            });
        }

        let mut commands = vec![self.parse_command()?];

        while self.current.token == Token::Pipe || self.current.token == Token::PipeAnd {
            self.advance();
            self.skip_newlines();
            commands.push(self.parse_command()?);
        }

        Ok(Pipeline {
            negated,
            commands,
            timed,
        })
    }

    fn parse_and_or(&mut self) -> Result<AndOrList, ParseError> {
        let first = self.parse_pipeline()?;
        let mut rest = Vec::new();

        loop {
            let conn = match &self.current.token {
                Token::And => Connector::And,
                Token::Or => Connector::Or,
                _ => break,
            };
            self.advance();
            self.skip_newlines();
            rest.push((conn, self.parse_pipeline()?));
        }

        Ok(AndOrList { first, rest })
    }

    fn parse_complete_command(&mut self) -> Result<CompleteCommand, ParseError> {
        let list = self.parse_and_or()?;
        let (background, disown) = match &self.current.token {
            Token::Amp => {
                self.advance();
                (true, false)
            }
            Token::AmpBang => {
                self.advance();
                (true, true)
            }
            _ => (false, false),
        };
        Ok(CompleteCommand {
            list,
            background,
            disown,
        })
    }

    fn parse_command_list(&mut self) -> Result<Vec<CompleteCommand>, ParseError> {
        let mut commands = Vec::new();
        self.skip_newlines();

        while self.is_command_start() {
            commands.push(self.parse_complete_command()?);
            // consume separators
            while self.current.token == Token::Semi || self.current.token == Token::Newline {
                self.advance();
            }
        }

        Ok(commands)
    }

    pub fn parse_program(&mut self) -> Result<Vec<CompleteCommand>, ParseError> {
        let parsed = self.parse_command_list();
        // A strict lexer preserves word tokens for downstream parsing, but records
        // any construct that reached EOF without its delimiter. Check that state
        // even when the recursive-descent parser produced another error first.
        if self.lexer.has_incomplete_construct() {
            return Err(ParseError::Incomplete);
        }
        let cmds = parsed?;
        if self.current.token != Token::Eof {
            return Err(ParseError::Unexpected(format!(
                "unexpected token {:?}",
                self.current.token
            )));
        }
        Ok(cmds)
    }
}

/// Check if input is syntactically incomplete (for multiline editing).
pub fn is_incomplete(input: &str) -> bool {
    // Use the real parser/lexer state so comment and here-doc text is not
    // mistaken for active shell syntax.
    let mut parser = Parser::new(input);
    matches!(parser.parse_program(), Err(ParseError::Incomplete))
}

pub fn parse(input: &str) -> Result<Vec<CompleteCommand>, ParseError> {
    // Try to get from cache
    if let Some(cached) = super::cache::cache_get(input) {
        return Ok(cached);
    }

    // Parse if not cached
    let result = Parser::new(input).parse_program();

    // Store in cache on success
    if let Ok(ref ast) = result {
        super::cache::cache_insert(input.to_string(), ast.clone());
    }

    result
}

// --- Helper functions ---

fn token_is_redirect(token: &Token) -> bool {
    matches!(
        token,
        Token::RedirectOut
            | Token::RedirectClobber
            | Token::RedirectAppend
            | Token::RedirectIn
            | Token::HereDoc
            | Token::HereDocStrip
            | Token::HereString
            | Token::DupFd
            | Token::RedirectAllOut
            | Token::RedirectAllAppend
            | Token::RedirectFd(_, _)
    )
}

/// The `name` of a `{name}>file` redirection: an identifier, optionally
/// with one array subscript (`{COPROC[1]}>&-`).
fn is_fd_variable_name(name: &str) -> bool {
    let (base, subscript) = match name.find('[') {
        Some(open) => match name[open + 1..].strip_suffix(']') {
            Some(subscript) if !subscript.is_empty() => (&name[..open], Some(subscript)),
            _ => return false,
        },
        None => (name, None),
    };
    let mut chars = base.chars();
    chars
        .next()
        .is_some_and(|c| c == '_' || c.is_ascii_alphabetic())
        && chars.all(|c| c == '_' || c.is_ascii_alphanumeric())
        && subscript.is_none_or(|s| !s.contains(['[', ']']))
}

fn is_reserved_word(w: &str) -> bool {
    matches!(
        w,
        "if" | "then"
            | "else"
            | "elif"
            | "fi"
            | "for"
            | "in"
            | "do"
            | "done"
            | "while"
            | "until"
            | "case"
            | "esac"
            | "function"
            | "!"
            | "{"
            | "}"
            // `[[ ( a ) ]]` is a conditional, not a function named `[[`.
            | "[["
    )
}

fn is_compound_keyword(w: &str) -> bool {
    matches!(w, "if" | "for" | "while" | "until" | "case" | "select")
}

/// Keywords that terminate a command list (not valid as the start of a new command).
fn is_list_terminator(w: &str) -> bool {
    matches!(
        w,
        "then" | "else" | "elif" | "fi" | "do" | "done" | "esac" | "}" | ")"
    )
}

fn is_assignment(w: &str) -> bool {
    // Support: VAR=value, VAR+=value, arr[idx]=value, arr=(...)
    let w_bytes = w.as_bytes();

    // Find the = sign (or += pattern)
    let mut i = 0;
    while i < w_bytes.len() {
        let c = w_bytes[i] as char;
        if c == '=' {
            // Everything before = must be a valid name (possibly with [idx])
            let before = &w[..i];
            return is_valid_assign_lhs(before);
        }
        if c == '+' && i + 1 < w_bytes.len() && w_bytes[i + 1] == b'=' {
            let before = &w[..i];
            return is_valid_assign_lhs(before);
        }
        if c == '[' {
            // skip to ] for array index
            while i < w_bytes.len() && w_bytes[i] != b']' {
                i += 1;
            }
        }
        i += 1;
    }
    false
}

fn is_valid_assign_lhs(s: &str) -> bool {
    if s.is_empty() {
        return false;
    }
    // Could be "name" or "name[idx]"
    let name = if let Some(bracket) = s.find('[') {
        if !s.ends_with(']') {
            return false;
        }
        &s[..bracket]
    } else {
        s
    };
    !name.is_empty()
        && name.chars().all(|c| c.is_alphanumeric() || c == '_')
        && !name.starts_with(|c: char| c.is_ascii_digit())
}

/// Split an assignment word at its operator: the first `=` or `+=` after the
/// name and its optional `[subscript]`, as `(lhs, append, value)`.
///
/// The operator has to be found from the left, past the name. Searching the
/// whole word for `+=` misread `x=a+=b` (whose value is `a+=b`) and every
/// compound assignment with a `[k]+=v` element.
pub fn split_assignment_word(w: &str) -> Option<(&str, bool, &str)> {
    let bytes = w.as_bytes();
    let mut i = 0;
    while i < bytes.len() && (bytes[i] == b'_' || bytes[i].is_ascii_alphanumeric()) {
        i += 1;
    }
    if i == 0 || bytes[0].is_ascii_digit() {
        return None;
    }
    if bytes.get(i) == Some(&b'[') {
        let mut depth = 0usize;
        loop {
            match bytes.get(i)? {
                b'[' => depth += 1,
                b']' => {
                    depth -= 1;
                    if depth == 0 {
                        i += 1;
                        break;
                    }
                }
                _ => {}
            }
            i += 1;
        }
    }
    match (bytes.get(i), bytes.get(i + 1)) {
        (Some(b'='), _) => Some((&w[..i], false, &w[i + 1..])),
        (Some(b'+'), Some(b'=')) => Some((&w[..i], true, &w[i + 2..])),
        _ => None,
    }
}

/// The parts of a compound assignment written as one word, `name=(...)` or
/// `name+=(...)`.
#[derive(Debug, Clone, PartialEq)]
pub struct CompoundAssignmentText<'a> {
    pub name: &'a str,
    pub append: bool,
    /// The element words, parsed but not yet expanded.
    pub elements: Vec<Word>,
}

/// Parse `name=(a "b c" [k]=v)` (or `name+=(...)`) into its name and element
/// words; `None` when the word is not a compound assignment to a plain name.
pub fn split_compound_assignment(text: &str) -> Option<CompoundAssignmentText<'_>> {
    let (lhs, append, value) = split_assignment_word(text)?;
    if !lhs.bytes().all(|b| b == b'_' || b.is_ascii_alphanumeric()) {
        return None;
    }
    let inner = value.strip_prefix('(')?.strip_suffix(')')?;
    if !quotes_are_closed(inner) {
        return None;
    }
    Some(CompoundAssignmentText {
        name: lhs,
        append,
        elements: split_array_elements(inner)
            .iter()
            .map(|element| parse_word_parts(element))
            .collect(),
    })
}

/// Does every quote in `text` close? A compound assignment that arrives as a
/// string (`declare -A 'm=([k]="v)'`) is re-parsed, and an unterminated
/// quote there must be an error rather than swallow the rest of the list.
fn quotes_are_closed(text: &str) -> bool {
    let mut quote: Option<char> = None;
    let mut chars = text.chars();
    while let Some(c) = chars.next() {
        match (quote, c) {
            (Some('\''), '\'') => quote = None,
            (Some('\''), _) => {}
            (_, '\\') => {
                chars.next();
            }
            (None, '\'' | '"') => quote = Some(c),
            (Some('"'), '"') => quote = None,
            _ => {}
        }
    }
    quote.is_none()
}

/// Is this the command word of a declaration builtin, whose arguments bash
/// parses as assignments (`local -a xs=(1 2)` declares an array)?
fn is_declaration_command(words: &[Word]) -> bool {
    matches!(
        words.first().map(Vec::as_slice),
        Some([WordPart::Literal(name)])
            if matches!(name.as_str(), "declare" | "typeset" | "local" | "export" | "readonly")
    )
}

fn parse_assignment(w: &str, parser: &mut Parser) -> Result<Assignment, ParseError> {
    // is_assignment() accepted the word, so it has an operator.
    let (before_eq, append, value_str) =
        split_assignment_word(w).ok_or_else(|| ParseError::Unexpected(w.to_string()))?;

    // Extract name and optional index from lhs
    let (name, index) = if let Some(bracket) = before_eq.find('[') {
        let idx = &before_eq[bracket + 1..before_eq.len() - 1]; // strip [ ]
        (&before_eq[..bracket], Some(idx.to_string()))
    } else {
        (before_eq, None)
    };

    // Check for array literal: name=(a b c)
    if value_str == "(" || (value_str.starts_with('(') && !value_str.ends_with(')')) {
        // Collect words until )
        let mut array_words = Vec::new();
        // If value_str is just "(", we need to read from parser
        let inner = if value_str == "(" {
            String::new()
        } else {
            value_str[1..].to_string()
        };

        // Parse any words already in the token
        if !inner.is_empty() {
            for part in split_array_elements(&inner) {
                array_words.push(parse_word_parts(&part));
            }
        }

        parser.advance(); // consume the assignment token

        // Read more words until )
        loop {
            match &parser.current.token {
                Token::RParen => {
                    parser.advance();
                    break;
                }
                Token::Word(w) => {
                    let w = w.clone();
                    // Check if word ends with )
                    if w.ends_with(')') {
                        let inner = &w[..w.len() - 1];
                        if !inner.is_empty() {
                            array_words.push(parse_word_parts(inner));
                        }
                        parser.advance();
                        break;
                    }
                    array_words.push(parse_word_parts(&w));
                    parser.advance();
                }
                Token::Eof => return Err(ParseError::Incomplete),
                _ => {
                    parser.advance();
                    break;
                }
            }
        }

        return Ok(Assignment {
            name: name.to_string(),
            value: vec![WordPart::Literal(String::new())],
            index: None,
            append,
            array_value: Some(array_words),
        });
    }

    // Check for complete array literal: name=(a b c) all in one token
    if value_str.starts_with('(') && value_str.ends_with(')') {
        let inner = &value_str[1..value_str.len() - 1];
        let array_words: Vec<Word> = split_array_elements(inner)
            .iter()
            .map(|s| parse_word_parts(s))
            .collect();
        parser.advance();
        return Ok(Assignment {
            name: name.to_string(),
            value: vec![WordPart::Literal(String::new())],
            index: None,
            append,
            array_value: Some(array_words),
        });
    }

    parser.advance();
    Ok(Assignment {
        name: name.to_string(),
        value: parse_word_parts(value_str),
        index,
        append,
        array_value: None,
    })
}

/// Brace groups nested deeper than this are left as literal text. Expanding
/// nested alternatives recurses once per level, and a group that does not
/// expand is rescanned from its inner `{`; the bound keeps a hostile word from
/// exhausting the stack or taking quadratic time. Real words nest a few levels.
const MAX_BRACE_NESTING: usize = 64;

/// The text of a `{...}` word segment, up to its matching `}`.
struct BraceBody {
    /// Text between the braces.
    content: String,
    /// The deepest nesting of brace groups, counting this one as 1.
    max_depth: usize,
    /// Byte offsets in `content` of the commas that separate alternatives:
    /// those outside quotes, escapes, `${...}`, `$(...)` and nested braces.
    commas: Vec<usize>,
    /// Whether `content` has a `:` at that same level. Brace alternatives
    /// never need one there, but a JSON object such as `{"name":"bob"}` does,
    /// and jsh keeps those words intact for its structured commands.
    top_level_colon: bool,
}

/// Scan a brace body the way bash's `brace_gobbler` does, starting just after
/// the `{`. Returns `None` when nothing closes it.
fn scan_brace_body(mut chars: std::iter::Peekable<std::str::Chars<'_>>) -> Option<BraceBody> {
    let mut body = BraceBody {
        content: String::new(),
        max_depth: 1,
        commas: Vec::new(),
        top_level_colon: false,
    };
    let mut depth = 1usize;
    while let Some(c) = chars.next() {
        let content = &mut body.content;
        match c {
            '\\' => {
                content.push(c);
                content.push(chars.next()?);
            }
            '\'' | '`' => {
                content.push(c);
                loop {
                    let c2 = chars.next()?;
                    content.push(c2);
                    if c2 == c {
                        break;
                    }
                }
            }
            '"' => {
                content.push(c);
                loop {
                    let c2 = chars.next()?;
                    content.push(c2);
                    if c2 == '\\' {
                        content.push(chars.next()?);
                    } else if c2 == '"' {
                        break;
                    }
                }
            }
            '$' if matches!(chars.peek(), Some('{' | '(')) => {
                // `${x:-a,b}` and `$(echo a,b)` are single alternatives.
                let open = chars.next()?;
                let close = if open == '{' { '}' } else { ')' };
                content.push(c);
                content.push(open);
                let mut nesting = 1usize;
                while nesting > 0 {
                    let c2 = chars.next()?;
                    content.push(c2);
                    if c2 == open {
                        nesting += 1;
                    } else if c2 == close {
                        nesting -= 1;
                    }
                }
            }
            '{' => {
                depth += 1;
                body.max_depth = body.max_depth.max(depth);
                content.push(c);
            }
            '}' => {
                depth -= 1;
                if depth == 0 {
                    return Some(body);
                }
                content.push(c);
            }
            ',' if depth == 1 => {
                body.commas.push(content.len());
                content.push(c);
            }
            ':' if depth == 1 => {
                body.top_level_colon = true;
                content.push(c);
            }
            _ => content.push(c),
        }
    }
    None
}

/// The alternatives of a comma brace expansion, each parsed as a word of its
/// own so it keeps its quoting and can nest further braces: `{a,'b,c'}`,
/// `x{a,b{1,2},c}y`, `a{,b}`.
fn brace_alternatives(brace: &BraceBody) -> Vec<Word> {
    // An empty alternative has no parts at all, so a word left with nothing
    // else disappears like any empty unquoted word (`{,}` is no words, as in
    // bash) while `a{,b}` still yields `a`.
    let alternative = |text: &str| {
        if text.is_empty() {
            Vec::new()
        } else {
            parse_word_parts(text)
        }
    };
    let mut alternatives = Vec::with_capacity(brace.commas.len() + 1);
    let mut start = 0;
    for &comma in &brace.commas {
        alternatives.push(alternative(&brace.content[start..comma]));
        start = comma + 1;
    }
    alternatives.push(alternative(&brace.content[start..]));
    alternatives
}

/// Detect `{|p1 p2 ...| body}` and split into (params, body_src).
/// Returns None if `raw` is not a closure literal.
fn try_parse_closure(raw: &str) -> Option<WordPart> {
    let bytes = raw.as_bytes();
    if bytes.len() < 4 || !raw.starts_with("{|") || !raw.ends_with('}') {
        return None;
    }
    let inner = &raw[2..raw.len() - 1]; // strip {| and }
                                        // Find the closing `|` for the params section. It's the first `|` at depth 0
                                        // (we already consumed the opening one), respecting nested quotes/braces.
    let mut depth: i32 = 0;
    let mut in_single = false;
    let mut in_double = false;
    let mut split: Option<usize> = None;
    let mut prev_escape = false;
    for (i, c) in inner.char_indices() {
        if prev_escape {
            prev_escape = false;
            continue;
        }
        if c == '\\' {
            prev_escape = true;
            continue;
        }
        if !in_double && c == '\'' {
            in_single = !in_single;
            continue;
        }
        if !in_single && c == '"' {
            in_double = !in_double;
            continue;
        }
        if in_single || in_double {
            continue;
        }
        match c {
            '{' => depth += 1,
            '}' => depth -= 1,
            '|' if depth == 0 => {
                split = Some(i);
                break;
            }
            _ => {}
        }
    }
    let split = split?;
    let params_str = &inner[..split];
    let body_src = inner[split + 1..].trim().to_string();
    let params: Vec<String> = params_str
        .split(|c: char| c.is_whitespace() || c == ',')
        .filter(|s| !s.is_empty())
        .map(|s| s.trim_start_matches('$').to_string())
        .collect();
    Some(WordPart::Closure { params, body_src })
}

/// Consume a backtick command substitution body, stopping at the first
/// unescaped backtick (which is consumed). Backslash escapes are preserved
/// verbatim for the later word-part stage. Inside backticks nothing else
/// nests, so a `"` here must not be read as opening a quote in the enclosing
/// scan — that is exactly what `"`dirname "$f"`"` relies on.
fn read_backtick_body(chars: &mut std::iter::Peekable<std::str::Chars<'_>>) -> String {
    let mut body = String::new();
    while let Some(c) = chars.next() {
        match c {
            '`' => break,
            '\\' => {
                body.push('\\');
                if let Some(next) = chars.next() {
                    body.push(next);
                }
            }
            _ => body.push(c),
        }
    }
    body
}

/// Read `$((...))` after its two opening parentheses. Ordinary nested
/// parentheses each have their own depth; only `))` at depth zero closes the
/// substitution. Pair-counting loses inner parentheses or includes the outer
/// terminator in the expression when arithmetic is nested or quoted.
fn read_arithmetic_sub(chars: &mut std::iter::Peekable<std::str::Chars<'_>>) -> String {
    let mut expression = String::new();
    let mut depth = 0usize;
    while let Some(ch) = chars.next() {
        match ch {
            '(' => depth += 1,
            ')' if depth == 0 && chars.peek() == Some(&')') => {
                chars.next();
                break;
            }
            ')' => depth = depth.saturating_sub(1),
            _ => {}
        }
        expression.push(ch);
    }
    expression
}

fn read_command_sub(chars: &mut std::iter::Peekable<std::str::Chars<'_>>) -> String {
    let mut cmd = String::new();
    let mut depth = 1;

    while let Some(c) = chars.next() {
        match c {
            '\\' => {
                cmd.push(c);
                if let Some(next) = chars.next() {
                    cmd.push(next);
                }
            }
            '\'' => {
                cmd.push(c);
                while let Some(next) = chars.next() {
                    cmd.push(next);
                    if next == '\'' {
                        break;
                    }
                    if next == '\\' {
                        if let Some(escaped) = chars.next() {
                            cmd.push(escaped);
                        }
                    }
                }
            }
            '"' => {
                cmd.push(c);
                while let Some(next) = chars.next() {
                    cmd.push(next);
                    if next == '\\' {
                        if let Some(escaped) = chars.next() {
                            cmd.push(escaped);
                        }
                        continue;
                    }
                    if next == '"' {
                        break;
                    }
                    if next == '`' {
                        cmd.push_str(&read_backtick_body(chars));
                        cmd.push('`');
                        continue;
                    }
                    if next == '$' && chars.peek() == Some(&'(') {
                        cmd.push(chars.next().unwrap());
                        let nested = read_command_sub(chars);
                        cmd.push_str(&nested);
                        cmd.push(')');
                    }
                }
            }
            '`' => {
                cmd.push(c);
                cmd.push_str(&read_backtick_body(chars));
                cmd.push('`');
            }
            '$' if chars.peek() == Some(&'(') => {
                cmd.push('$');
                cmd.push(chars.next().unwrap());
                let nested = read_command_sub(chars);
                cmd.push_str(&nested);
                cmd.push(')');
            }
            '(' => {
                depth += 1;
                cmd.push(c);
            }
            ')' => {
                depth -= 1;
                if depth == 0 {
                    break;
                }
                cmd.push(c);
            }
            _ => cmd.push(c),
        }
    }

    cmd
}

fn read_parameter_expansion(chars: &mut std::iter::Peekable<std::str::Chars<'_>>) -> String {
    let mut inner = String::new();
    let mut depth = 1;
    let mut in_single = false;
    let mut in_double = false;

    while let Some(c) = chars.next() {
        if in_single {
            inner.push(c);
            if c == '\'' {
                in_single = false;
            }
            continue;
        }

        if in_double {
            inner.push(c);
            match c {
                '\\' => {
                    if let Some(next) = chars.next() {
                        inner.push(next);
                    }
                }
                '"' => in_double = false,
                _ => {}
            }
            continue;
        }

        match c {
            '\\' => {
                inner.push(c);
                if let Some(next) = chars.next() {
                    inner.push(next);
                }
            }
            '\'' => {
                in_single = true;
                inner.push(c);
            }
            '"' => {
                in_double = true;
                inner.push(c);
            }
            // Only `${` opens a nested expansion. A bare `{` is an ordinary
            // pattern character, and counting it as an opener stole the closing
            // brace of expansions such as `${opt%%[<{().[]*}`, leaving the
            // pattern with a trailing `}` that matched nothing.
            '$' if chars.peek() == Some(&'{') => {
                inner.push(c);
                inner.push(chars.next().unwrap());
                depth += 1;
            }
            '$' if chars.peek() == Some(&'(') => {
                inner.push(c);
                inner.push(chars.next().unwrap());
                inner.push_str(&read_command_sub(chars));
                inner.push(')');
            }
            '}' => {
                depth -= 1;
                if depth == 0 {
                    break;
                }
                inner.push(c);
            }
            _ => inner.push(c),
        }
    }

    inner
}

fn read_double_quoted(chars: &mut std::iter::Peekable<std::str::Chars<'_>>) -> String {
    let mut inner = String::new();

    while let Some(c) = chars.next() {
        match c {
            '"' => break,
            '\\' => {
                inner.push('\\');
                if let Some(next) = chars.next() {
                    inner.push(next);
                }
            }
            '$' if chars.peek() == Some(&'(') => {
                inner.push('$');
                inner.push(chars.next().unwrap());
                let nested = read_command_sub(chars);
                inner.push_str(&nested);
                inner.push(')');
            }
            '$' if chars.peek() == Some(&'{') => {
                inner.push('$');
                inner.push(chars.next().unwrap());
                let nested = read_parameter_expansion(chars);
                inner.push_str(&nested);
                inner.push('}');
            }
            '`' => {
                inner.push('`');
                inner.push_str(&read_backtick_body(chars));
                inner.push('`');
            }
            _ => inner.push(c),
        }
    }

    inner
}

/// After a `$name`, try to consume a chain of `.field` / `[N]` segments
/// (nushell-style path access). Returns the empty vec if nothing matches.
/// Only consumes characters that look like a path: `.` must be followed by
/// an alphanumeric or `_`; `[` must contain digits (optionally `-`) and `]`.
/// Otherwise we leave the chars alone so bash-style `$name.txt` keeps working.
fn try_read_path(chars: &mut std::iter::Peekable<std::str::Chars<'_>>) -> Vec<PathSeg> {
    let mut path = Vec::new();
    loop {
        match chars.peek() {
            Some(&'.') => {
                let mut probe = chars.clone();
                probe.next();
                let ok = matches!(probe.peek(), Some(&c) if c.is_alphanumeric() || c == '_');
                if !ok {
                    break;
                }
                chars.next();
                let mut field = String::new();
                while let Some(&c) = chars.peek() {
                    if c.is_alphanumeric() || c == '_' {
                        field.push(c);
                        chars.next();
                    } else {
                        break;
                    }
                }
                if !field.is_empty() && field.chars().all(|c| c.is_ascii_digit()) {
                    if let Ok(n) = field.parse::<i64>() {
                        path.push(PathSeg::Index(n));
                        continue;
                    }
                }
                path.push(PathSeg::Field(field));
            }
            Some(&'[') => {
                let mut probe = chars.clone();
                probe.next();
                let neg = probe.peek() == Some(&'-');
                if neg {
                    probe.next();
                }
                let mut idx = String::new();
                let mut closed = false;
                while let Some(&c) = probe.peek() {
                    if c == ']' {
                        closed = true;
                        break;
                    }
                    if c.is_ascii_digit() {
                        idx.push(c);
                        probe.next();
                    } else {
                        break;
                    }
                }
                if !closed || idx.is_empty() {
                    break;
                }
                chars.next();
                if neg {
                    chars.next();
                }
                for _ in 0..idx.len() {
                    chars.next();
                }
                chars.next();
                let n: i64 = idx.parse().unwrap_or(0);
                path.push(PathSeg::Index(if neg { -n } else { n }));
            }
            _ => break,
        }
    }
    path
}

/// Read body of a `$"...($expr)..."` interpolated string. Caller has already
/// consumed the opening `$"`. Stops at the closing `"`.
fn read_interpolated(chars: &mut std::iter::Peekable<std::str::Chars<'_>>) -> Vec<InterpPart> {
    let mut parts = Vec::new();
    let mut lit = String::new();
    while let Some(&c) = chars.peek() {
        match c {
            '"' => {
                chars.next();
                break;
            }
            '\\' => {
                chars.next();
                match chars.next() {
                    Some(c2 @ ('\\' | '"' | '(' | '$')) => lit.push(c2),
                    Some('n') => lit.push('\n'),
                    Some('t') => lit.push('\t'),
                    Some('r') => lit.push('\r'),
                    Some(other) => {
                        lit.push('\\');
                        lit.push(other);
                    }
                    None => lit.push('\\'),
                }
            }
            '(' => {
                if !lit.is_empty() {
                    parts.push(InterpPart::Lit(std::mem::take(&mut lit)));
                }
                chars.next();
                let mut body = String::new();
                let mut depth = 1;
                while let Some(&c2) = chars.peek() {
                    chars.next();
                    if c2 == '(' {
                        depth += 1;
                        body.push(c2);
                        continue;
                    }
                    if c2 == ')' {
                        depth -= 1;
                        if depth == 0 {
                            break;
                        }
                        body.push(c2);
                        continue;
                    }
                    body.push(c2);
                }
                parts.push(InterpPart::Expr(parse_word_parts(&body)));
            }
            _ => {
                lit.push(c);
                chars.next();
            }
        }
    }
    if !lit.is_empty() {
        parts.push(InterpPart::Lit(lit));
    }
    parts
}

pub fn parse_word_parts(raw: &str) -> Word {
    // Closure literal `{|p1 p2| body}` — lexer hands us the entire thing as one
    // word. Decompose into params + body_src.
    if let Some(closure) = try_parse_closure(raw) {
        return vec![closure];
    }
    let mut parts = Vec::new();
    let mut chars = raw.chars().peekable();
    let mut literal = String::new();

    while let Some(&c) = chars.peek() {
        match c {
            '\'' => {
                if !literal.is_empty() {
                    parts.push(WordPart::Literal(std::mem::take(&mut literal)));
                }
                chars.next();
                let mut s = String::new();
                while let Some(&c2) = chars.peek() {
                    if c2 == '\'' {
                        chars.next();
                        break;
                    }
                    s.push(c2);
                    chars.next();
                }
                parts.push(WordPart::SingleQuoted(s));
            }
            '"' => {
                if !literal.is_empty() {
                    parts.push(WordPart::Literal(std::mem::take(&mut literal)));
                }
                chars.next();
                let inner = read_double_quoted(&mut chars);
                parts.push(WordPart::DoubleQuoted(parse_word_parts_inner(&inner)));
            }
            '$' => {
                if !literal.is_empty() {
                    parts.push(WordPart::Literal(std::mem::take(&mut literal)));
                }
                chars.next();
                match chars.peek() {
                    Some(&'\'') => {
                        // ANSI-C quoting $'...' -- decode escapes, no further expansion.
                        chars.next();
                        let mut raw = String::new();
                        while let Some(&c2) = chars.peek() {
                            if c2 == '\\' {
                                raw.push('\\');
                                chars.next();
                                if let Some(&c3) = chars.peek() {
                                    raw.push(c3);
                                    chars.next();
                                }
                                continue;
                            }
                            if c2 == '\'' {
                                chars.next();
                                break;
                            }
                            raw.push(c2);
                            chars.next();
                        }
                        parts.push(WordPart::SingleQuoted(decode_ansi_c(&raw)));
                    }
                    Some(&'(') => {
                        chars.next();
                        if chars.peek() == Some(&'(') {
                            // Arithmetic $((...))
                            chars.next();
                            let expr = read_arithmetic_sub(&mut chars);
                            parts.push(WordPart::Arithmetic(expr));
                        } else {
                            // Command substitution $(...)
                            let cmd = read_command_sub(&mut chars);
                            parts.push(WordPart::CommandSub(cmd));
                        }
                    }
                    Some(&'{') => {
                        chars.next();
                        // Stopping at the first `}` cut `${x:-${y}}` short and
                        // left the extra brace in the word.
                        parts.push(WordPart::Variable(read_parameter_expansion(&mut chars)));
                    }
                    Some(&'"') => {
                        chars.next();
                        let interp = read_interpolated(&mut chars);
                        parts.push(WordPart::Interpolated(interp));
                    }
                    Some(&c2)
                        if c2.is_alphanumeric()
                            || c2 == '_'
                            || c2 == '?'
                            || c2 == '$'
                            || c2 == '!'
                            || c2 == '#'
                            || c2 == '@'
                            || c2 == '-'
                            || c2 == '*' =>
                    {
                        let mut var = String::new();
                        // `$10` is `${1}0`: an unbraced positional parameter
                        // is a single digit.
                        if "?$!#@*-".contains(c2) || c2.is_ascii_digit() {
                            var.push(c2);
                            chars.next();
                        } else {
                            while let Some(&c3) = chars.peek() {
                                if c3.is_alphanumeric() || c3 == '_' {
                                    var.push(c3);
                                    chars.next();
                                } else {
                                    break;
                                }
                            }
                        }
                        let path = try_read_path(&mut chars);
                        if path.is_empty() {
                            parts.push(WordPart::Variable(var));
                        } else {
                            parts.push(WordPart::VariablePath { name: var, path });
                        }
                    }
                    _ => {
                        literal.push('$');
                    }
                }
            }
            '`' => {
                if !literal.is_empty() {
                    parts.push(WordPart::Literal(std::mem::take(&mut literal)));
                }
                chars.next();
                let mut cmd = String::new();
                while let Some(&c2) = chars.peek() {
                    if c2 == '`' {
                        chars.next();
                        break;
                    }
                    if c2 == '\\' {
                        chars.next();
                        if let Some(&c3) = chars.peek() {
                            cmd.push(c3);
                            chars.next();
                        }
                        continue;
                    }
                    cmd.push(c2);
                    chars.next();
                }
                parts.push(WordPart::CommandSub(cmd));
            }
            '~' if literal.is_empty() && parts.is_empty() => {
                chars.next();
                let mut user = String::new();
                while let Some(&c2) = chars.peek() {
                    if c2 == '/' || c2 == ':' {
                        break;
                    }
                    user.push(c2);
                    chars.next();
                }
                parts.push(WordPart::Tilde(user));
            }
            '<' | '>' if chars.clone().nth(1) == Some('(') => {
                // Process substitution: <(cmd) or >(cmd)
                if !literal.is_empty() {
                    parts.push(WordPart::Literal(std::mem::take(&mut literal)));
                }
                let kind = if c == '<' {
                    ProcessSubKind::Input
                } else {
                    ProcessSubKind::Output
                };
                chars.next(); // consume < or >
                chars.next(); // consume (
                let mut cmd = String::new();
                let mut depth = 1;
                while let Some(&c2) = chars.peek() {
                    if c2 == '(' {
                        depth += 1;
                    }
                    if c2 == ')' {
                        depth -= 1;
                        if depth == 0 {
                            chars.next();
                            break;
                        }
                    }
                    cmd.push(c2);
                    chars.next();
                }
                parts.push(WordPart::ProcessSub(cmd, kind));
            }
            '*' | '?' | '[' => {
                if !literal.is_empty() {
                    parts.push(WordPart::Literal(std::mem::take(&mut literal)));
                }
                let mut glob = String::new();
                glob.push(c);
                chars.next();
                if c == '[' {
                    while let Some(&c2) = chars.peek() {
                        glob.push(c2);
                        chars.next();
                        if c2 == ']' {
                            break;
                        }
                    }
                }
                parts.push(WordPart::Glob(glob));
            }
            '\\' => {
                chars.next();
                if let Some(&c2) = chars.peek() {
                    literal.push(c2);
                    chars.next();
                }
            }
            '{' => {
                // Brace expansion: `{a,b,c}` or a sequence such as `{1..10}`.
                // Any other `{` is literal text and parsing carries on inside
                // it, so `{a{b,c}}` still expands its inner pair and `{$x}`
                // still expands `$x`, as in bash.
                chars.next(); // consume '{'
                let Some(brace) = scan_brace_body(chars.clone()) else {
                    literal.push('{');
                    continue;
                };
                let part = if let Some(range) = parse_brace_range(&brace.content) {
                    range
                } else if brace.top_level_colon || brace.max_depth > MAX_BRACE_NESTING {
                    // A JSON object (`let u = {"name":"bob"}`) stays opaque,
                    // and so does a group nested beyond the expansion bound.
                    literal.push('{');
                    literal.push_str(&brace.content);
                    literal.push('}');
                    chars.nth(brace.content.chars().count());
                    continue;
                } else if !brace.commas.is_empty() {
                    WordPart::BraceExpansion(brace_alternatives(&brace))
                } else {
                    literal.push('{');
                    continue;
                };
                if !literal.is_empty() {
                    parts.push(WordPart::Literal(std::mem::take(&mut literal)));
                }
                parts.push(part);
                chars.nth(brace.content.chars().count()); // the body and its `}`
            }
            _ => {
                literal.push(c);
                chars.next();
            }
        }
    }

    if !literal.is_empty() {
        parts.push(WordPart::Literal(literal));
    }

    if parts.is_empty() {
        parts.push(WordPart::Literal(String::new()));
    }

    parts
}

fn parse_word_parts_inner(input: &str) -> Vec<WordPart> {
    parse_double_quoted_body(input, false)
}

/// Parse arithmetic text (`$((...))`, `((...))`, a `for ((...))` clause or an
/// array subscript) for the expansion bash performs before evaluating it:
/// the text expands as if inside double quotes, except that unescaped double
/// quotes are dropped rather than kept, so `$(( "1" + 2 ))` is 3.
pub fn parse_arithmetic_parts(expr: &str) -> Vec<WordPart> {
    parse_double_quoted_body(expr, true)
}

fn parse_double_quoted_body(input: &str, arithmetic: bool) -> Vec<WordPart> {
    let mut parts = Vec::new();
    let mut chars = input.chars().peekable();
    let mut literal = String::new();

    while let Some(&c) = chars.peek() {
        if c == '\\' {
            // Inside double quotes, backslash only escapes $ ` " \ (and newline).
            chars.next();
            match chars.peek() {
                Some(&c2 @ ('$' | '`' | '"' | '\\')) => {
                    literal.push(c2);
                    chars.next();
                }
                Some(&'\n') => {
                    chars.next();
                } // line continuation
                _ => literal.push('\\'),
            }
        } else if c == '`' {
            if !literal.is_empty() {
                parts.push(WordPart::Literal(std::mem::take(&mut literal)));
            }
            chars.next();
            let mut cmd = String::new();
            while let Some(&c2) = chars.peek() {
                if c2 == '`' {
                    chars.next();
                    break;
                }
                if c2 == '\\' {
                    chars.next();
                    if let Some(&c3) = chars.peek() {
                        cmd.push(c3);
                        chars.next();
                    }
                    continue;
                }
                cmd.push(c2);
                chars.next();
            }
            parts.push(WordPart::CommandSub(cmd));
        } else if c == '$' {
            if !literal.is_empty() {
                parts.push(WordPart::Literal(std::mem::take(&mut literal)));
            }
            chars.next();
            match chars.peek() {
                Some(&c2)
                    if c2.is_alphanumeric()
                        || c2 == '_'
                        || c2 == '?'
                        || c2 == '$'
                        || c2 == '!'
                        || c2 == '#'
                        || c2 == '@'
                        || c2 == '-'
                        || c2 == '*' =>
                {
                    let mut var = String::new();
                    // `$10` is `${1}0`, as at the top level of a word.
                    if "?$!#@*-".contains(c2) || c2.is_ascii_digit() {
                        var.push(c2);
                        chars.next();
                    } else {
                        while let Some(&c3) = chars.peek() {
                            if c3.is_alphanumeric() || c3 == '_' {
                                var.push(c3);
                                chars.next();
                            } else {
                                break;
                            }
                        }
                    }
                    let path = try_read_path(&mut chars);
                    if path.is_empty() {
                        parts.push(WordPart::Variable(var));
                    } else {
                        parts.push(WordPart::VariablePath { name: var, path });
                    }
                }
                Some(&'{') => {
                    chars.next();
                    parts.push(WordPart::Variable(read_parameter_expansion(&mut chars)));
                }
                Some(&'(') => {
                    chars.next();
                    if chars.peek() == Some(&'(') {
                        // Arithmetic $((...))
                        chars.next();
                        let expr = read_arithmetic_sub(&mut chars);
                        parts.push(WordPart::Arithmetic(expr));
                    } else {
                        // Command substitution $(...)
                        let cmd = read_command_sub(&mut chars);
                        parts.push(WordPart::CommandSub(cmd));
                    }
                }
                _ => literal.push('$'),
            }
        } else if arithmetic && c == '"' {
            chars.next();
        } else {
            literal.push(c);
            chars.next();
        }
    }

    if !literal.is_empty() {
        parts.push(WordPart::Literal(literal));
    }
    parts
}

/// Decode ANSI-C escape sequences for $'...' quoting.
pub(crate) fn decode_ansi_c(s: &str) -> String {
    let mut out = String::new();
    let mut chars = s.chars().peekable();
    while let Some(c) = chars.next() {
        if c != '\\' {
            out.push(c);
            continue;
        }
        match chars.next() {
            Some('n') => out.push('\n'),
            Some('t') => out.push('\t'),
            Some('r') => out.push('\r'),
            Some('a') => out.push('\u{07}'),
            Some('b') => out.push('\u{08}'),
            Some('e') | Some('E') => out.push('\u{1b}'),
            Some('f') => out.push('\u{0c}'),
            Some('v') => out.push('\u{0b}'),
            Some('\\') => out.push('\\'),
            Some('\'') => out.push('\''),
            Some('"') => out.push('"'),
            Some('?') => out.push('?'),
            Some('x') => {
                let mut hex = String::new();
                while hex.len() < 2 {
                    match chars.peek() {
                        Some(&h) if h.is_ascii_hexdigit() => {
                            hex.push(h);
                            chars.next();
                        }
                        _ => break,
                    }
                }
                if let Ok(n) = u32::from_str_radix(&hex, 16) {
                    if let Some(ch) = char::from_u32(n) {
                        out.push(ch);
                    }
                } else {
                    out.push('\\');
                    out.push('x');
                    out.push_str(&hex);
                }
            }
            Some('u') => {
                let mut hex = String::new();
                while hex.len() < 4 {
                    match chars.peek() {
                        Some(&h) if h.is_ascii_hexdigit() => {
                            hex.push(h);
                            chars.next();
                        }
                        _ => break,
                    }
                }
                if let Ok(n) = u32::from_str_radix(&hex, 16) {
                    if let Some(ch) = char::from_u32(n) {
                        out.push(ch);
                    }
                }
            }
            Some(c @ '0'..='7') => {
                let mut oct = String::new();
                oct.push(c);
                while oct.len() < 3 {
                    match chars.peek() {
                        Some(&o) if ('0'..='7').contains(&o) => {
                            oct.push(o);
                            chars.next();
                        }
                        _ => break,
                    }
                }
                if let Ok(n) = u32::from_str_radix(&oct, 8) {
                    if let Some(ch) = char::from_u32(n) {
                        out.push(ch);
                    }
                }
            }
            Some(other) => {
                out.push('\\');
                out.push(other);
            }
            None => out.push('\\'),
        }
    }
    out
}

/// A sequence expression `{x..y}` or `{x..y..step}`. Both ends must be
/// integers or both single letters, and a step must be an integer; anything
/// else is no sequence, and bash leaves it as literal text (`{1..x}`).
fn parse_brace_range(content: &str) -> Option<WordPart> {
    let parts: Vec<&str> = content.split("..").collect();
    let (start, end, step) = match parts.as_slice() {
        [start, end] => (*start, *end, None),
        [start, end, step] => (*start, *end, Some(*step)),
        _ => return None,
    };
    let is_integer = |text: &str| text.parse::<i64>().is_ok();
    let is_letter = |text: &str| text.len() == 1 && text.as_bytes()[0].is_ascii_alphabetic();
    let ends_match = (is_integer(start) && is_integer(end)) || (is_letter(start) && is_letter(end));
    if !ends_match || step.is_some_and(|step| !is_integer(step)) {
        return None;
    }
    Some(WordPart::BraceRange {
        start: start.to_string(),
        end: end.to_string(),
        step: step.map(str::to_string),
    })
}

#[cfg(test)]
mod tests {
    use super::{
        parse, parse_arithmetic_parts, parse_word_parts, scan_double_paren, Command,
        CompoundCommand, DoubleParenEnd, WordPart,
    };

    /// The single command of a one-command program.
    fn only_command(src: &str) -> Command {
        let mut program = parse(src).unwrap_or_else(|e| panic!("{src}: {e}"));
        assert_eq!(program.len(), 1, "{src}");
        let list = program.remove(0).list;
        assert!(list.rest.is_empty(), "{src}");
        let mut commands = list.first.commands;
        assert_eq!(commands.len(), 1, "{src}");
        commands.remove(0)
    }

    #[test]
    fn double_paren_scan_finds_the_matching_close() {
        assert_eq!(
            scan_double_paren("x<=3)) && y"),
            DoubleParenEnd::Arithmetic(4)
        );
        assert_eq!(
            scan_double_paren(" (1+2) * 3 ))"),
            DoubleParenEnd::Arithmetic(11)
        );
        assert_eq!(
            scan_double_paren("$((1)) + $(echo ')')))"),
            DoubleParenEnd::Arithmetic(20)
        );
        assert_eq!(
            scan_double_paren("echo hi) | cat)"),
            DoubleParenEnd::Subshells
        );
        assert_eq!(scan_double_paren("x ) )"), DoubleParenEnd::Subshells);
        assert_eq!(scan_double_paren("x = (1"), DoubleParenEnd::Incomplete);
        assert_eq!(scan_double_paren("\"))"), DoubleParenEnd::Incomplete);
    }

    #[test]
    fn arithmetic_command_text_bypasses_the_shell_lexer() {
        for (src, expr) in [
            ("((i>=3))", "i>=3"),
            ("((x<<=2, y>>=1))", "x<<=2, y>>=1"),
            ("(( $1 <= 1 ))", " $1 <= 1 "),
            ("((\n  x = 1\n))", "\n  x = 1\n"),
        ] {
            match only_command(src) {
                Command::Compound(CompoundCommand::Arithmetic { expr: got, .. }) => {
                    assert_eq!(got, expr, "{src}")
                }
                other => panic!("{src}: {other:?}"),
            }
        }
    }

    #[test]
    fn double_parens_that_do_not_close_together_are_subshells() {
        for src in ["((echo hi) | cat)", "( (echo hi) )", "( (1+2) )"] {
            match only_command(src) {
                Command::Compound(CompoundCommand::Subshell { .. }) => {}
                other => panic!("{src}: {other:?}"),
            }
        }
    }

    #[test]
    fn arithmetic_for_header_keeps_operators() {
        match only_command("for ((i=1; i<=64; i<<=1)); do :; done") {
            Command::Compound(CompoundCommand::CStyleFor {
                init,
                condition,
                update,
                ..
            }) => assert_eq!(
                (init, condition, update),
                ("i=1".into(), "i<=64".into(), "i<<=1".into())
            ),
            other => panic!("{other:?}"),
        }
        assert!(matches!(
            parse("for ((i=0; i<3; i++) ); do :; done"),
            Err(super::ParseError::Unexpected(_))
        ));
        assert!(super::is_incomplete("for ((i=0;\n"));
        assert!(super::is_incomplete("((x = 1 +\n"));
    }

    #[test]
    fn arithmetic_text_drops_bare_double_quotes_only() {
        assert_eq!(
            parse_arithmetic_parts(r#""1" + $x"#),
            vec![
                WordPart::Literal("1 + ".into()),
                WordPart::Variable("x".into())
            ]
        );
        assert_eq!(
            parse_arithmetic_parts(r#"\"1\" + '2'"#),
            vec![WordPart::Literal(r#""1" + '2'"#.into())]
        );
    }

    #[test]
    fn brace_alternatives_split_on_top_level_commas() {
        let literal = |s: &str| vec![WordPart::Literal(s.into())];
        assert_eq!(
            parse_word_parts("{a,{b,c}}"),
            vec![WordPart::BraceExpansion(vec![
                literal("a"),
                vec![WordPart::BraceExpansion(vec![literal("b"), literal("c")])],
            ])]
        );
        assert_eq!(
            parse_word_parts("{a,'b,c'}"),
            vec![WordPart::BraceExpansion(vec![
                literal("a"),
                vec![WordPart::SingleQuoted("b,c".into())],
            ])]
        );
        assert_eq!(
            parse_word_parts("a{,b}"),
            vec![
                WordPart::Literal("a".into()),
                WordPart::BraceExpansion(vec![vec![], literal("b")]),
            ]
        );
        // Only the inner pair expands; the outer braces are text.
        assert_eq!(
            parse_word_parts("{a{b,c}}"),
            vec![
                WordPart::Literal("{a".into()),
                WordPart::BraceExpansion(vec![literal("b"), literal("c")]),
                WordPart::Literal("}".into()),
            ]
        );
        // JSON objects stay opaque, quotes and all.
        assert_eq!(
            parse_word_parts(r#"{"a":1,"b":2}"#),
            literal(r#"{"a":1,"b":2}"#)
        );
    }

    /// Groups nested past `MAX_BRACE_NESTING` stay text instead of recursing.
    #[test]
    fn brace_nesting_is_bounded() {
        let nested = |depth: usize| format!("{}b{}", "{a,".repeat(depth), "}".repeat(depth));
        assert!(matches!(
            parse_word_parts(&nested(super::MAX_BRACE_NESTING)).as_slice(),
            [WordPart::BraceExpansion(_)]
        ));
        let deep = nested(super::MAX_BRACE_NESTING + 1);
        assert_eq!(
            parse_word_parts(&deep),
            vec![WordPart::Literal(deep.clone())]
        );
    }

    #[test]
    fn invalid_sequences_stay_literal() {
        for word in [
            "{1..x}",
            "{a..5}",
            "{1..3..x}",
            "{..3}",
            "{1...3}",
            "{1..2..3..4}",
        ] {
            assert_eq!(
                parse_word_parts(word),
                vec![WordPart::Literal(word.into())],
                "{word}"
            );
        }
        assert!(matches!(
            parse_word_parts("{-2..2}").as_slice(),
            [WordPart::BraceRange { .. }]
        ));
        assert!(matches!(
            parse_word_parts("{a..j..3}").as_slice(),
            [WordPart::BraceRange { .. }]
        ));
    }

    #[test]
    fn assignment_operator_is_found_after_the_name() {
        use super::split_assignment_word;
        assert_eq!(split_assignment_word("x=a+=b"), Some(("x", false, "a+=b")));
        assert_eq!(split_assignment_word("x+=1"), Some(("x", true, "1")));
        assert_eq!(
            split_assignment_word("a[i+=1]=v"),
            Some(("a[i+=1]", false, "v"))
        );
        assert_eq!(
            split_assignment_word("m=([k]+=v)"),
            Some(("m", false, "([k]+=v)"))
        );
        assert_eq!(split_assignment_word("1x=2"), None);
        assert_eq!(split_assignment_word("=x"), None);
    }

    #[test]
    fn compound_assignment_elements_keep_quoting_and_substitutions() {
        use super::split_compound_assignment;
        let parsed = split_compound_assignment(r#"a=("x y" $(echo "1 2") [k]=v)"#).unwrap();
        assert_eq!(parsed.name, "a");
        assert!(!parsed.append);
        assert_eq!(parsed.elements.len(), 3);
        assert!(split_compound_assignment(r#"a=("unterminated)"#).is_none());
        assert!(split_compound_assignment("a=scalar").is_none());
    }

    #[test]
    fn declaration_builtins_record_compound_arguments() {
        use crate::parser::ast::Command;
        let cmds = parse(r#"local -a xs=(a "b c") y=1"#).unwrap();
        let Command::Simple(simple) = &cmds[0].list.first.commands[0] else {
            panic!("expected a simple command");
        };
        assert_eq!(
            simple.compound_args,
            vec![(2, r#"xs=(a "b c")"#.to_string())]
        );
        // Anything else keeps an empty table.
        let cmds = parse(r#"echo xs=(a)"#);
        if let Ok(cmds) = cmds {
            if let Command::Simple(simple) = &cmds[0].list.first.commands[0] {
                assert!(simple.compound_args.is_empty());
            }
        }
    }

    #[test]
    fn command_substitution_keeps_nested_quoted_subshells() {
        let parts = parse_word_parts(r#"$(dirname "$(dirname "$CONDA_EXE")")"#);
        assert_eq!(
            parts,
            vec![WordPart::CommandSub(
                r#"dirname "$(dirname "$CONDA_EXE")""#.to_string()
            )]
        );
    }

    #[test]
    fn double_quoted_command_substitution_keeps_inner_quotes() {
        let parts = parse_word_parts(r#""$(dirname "$CONDA_EXE")""#);
        assert_eq!(
            parts,
            vec![WordPart::DoubleQuoted(vec![WordPart::CommandSub(
                r#"dirname "$CONDA_EXE""#.to_string()
            )])]
        );
    }

    #[test]
    fn if_body_with_nested_command_sub_and_parameter_expansion_parses() {
        let src = "if true; then\n\
                    PATH=\"$(\\dirname \"$(\\dirname \"$D\")\")/condabin${PATH:+\":${PATH}\"}\"\n\
                    echo done\n\
                   fi\n";
        assert!(parse(src).is_ok(), "{:?}", parse(src));
    }

    fn simple_commands(src: &str) -> Vec<crate::parser::ast::SimpleCommand> {
        use crate::parser::ast::{Command, CompoundCommand};
        fn walk(
            commands: &[crate::parser::ast::CompleteCommand],
            out: &mut Vec<crate::parser::ast::SimpleCommand>,
        ) {
            for complete in commands {
                let pipelines = std::iter::once(&complete.list.first)
                    .chain(complete.list.rest.iter().map(|(_, p)| p));
                for pipeline in pipelines {
                    for command in &pipeline.commands {
                        match command {
                            Command::Simple(simple) => out.push(simple.clone()),
                            Command::FunctionDef { body, .. } => {
                                if let CompoundCommand::BraceGroup { body, .. } = body.as_ref() {
                                    walk(body, out);
                                }
                            }
                            Command::Compound(CompoundCommand::BraceGroup { body, .. }) => {
                                walk(body, out)
                            }
                            Command::Compound(_) => {}
                        }
                    }
                }
            }
        }
        let mut out = Vec::new();
        walk(&parse(src).expect("parse"), &mut out);
        out
    }

    /// Lines follow bash: a plain first word is reported on the line where
    /// the token after it ends; an assignment or redirection on its own.
    #[test]
    fn simple_commands_carry_bash_line_numbers() {
        let src =
            "echo a \\\n  b\nx=1 \\\n  y=2\necho \"m\nl\"\nf() {\n  g\n}\ncat <<E\nbody\nE\nlast";
        let lines: Vec<usize> = simple_commands(src).iter().map(|c| c.line).collect();
        assert_eq!(lines, [1, 3, 6, 8, 10, 13]);
    }

    /// `BASH_COMMAND` text: tokens as written, one space apart, with
    /// descriptor duplications and here-document operators kept whole.
    #[test]
    fn simple_commands_carry_their_text() {
        let texts: Vec<String> =
            simple_commands("echo   \"a  b\"  2>&1 >/dev/null <<E\nx\nE\nexec {fd}>out")
                .into_iter()
                .map(|c| c.text)
                .collect();
        assert_eq!(
            texts,
            ["echo \"a  b\" 2>&1 > /dev/null <<E", "exec {fd}> out"]
        );
    }

    #[test]
    fn descriptor_variables_and_coproc_and_time_parse() {
        use crate::parser::ast::{Command, CompoundCommand, TimeFormat};
        let program = parse(
            "exec {fd}>&- {arr[1]}<in; x {y}2>z; coproc NAME { cat; }; coproc cat; time -p true",
        )
        .expect("parse");
        let Command::Simple(exec) = &program[0].list.first.commands[0] else {
            panic!("exec is a simple command");
        };
        let names: Vec<_> = exec.redirects.iter().map(|r| r.fd_var.clone()).collect();
        assert_eq!(names, [Some("fd".to_string()), Some("arr[1]".to_string())]);
        // `{y}2>z` is an argument followed by an ordinary `2>` redirection.
        let Command::Simple(plain) = &program[1].list.first.commands[0] else {
            panic!("simple command");
        };
        assert_eq!(plain.words.len(), 2);
        assert_eq!(plain.redirects[0].fd_var, None);
        for (index, expected) in [(2, Some("NAME")), (3, None)] {
            let Command::Compound(CompoundCommand::Coproc { name, .. }) =
                &program[index].list.first.commands[0]
            else {
                panic!("coproc");
            };
            assert_eq!(name.as_deref(), expected);
        }
        assert_eq!(program[4].list.first.timed, Some(TimeFormat::Posix));
    }
}
