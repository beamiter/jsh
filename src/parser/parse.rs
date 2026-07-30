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

/// Split the body of an array literal (`arr=(...)`) into element words. Quoting
/// is honored, so `("a b" c)` is two elements and not three, and `#` starting a
/// word begins a comment as it does in bash.
fn split_array_elements(s: &str) -> Vec<String> {
    let mut elems = Vec::new();
    let mut cur = String::new();
    // Pending closers for `$( ... )` / `${ ... }`, inside which whitespace does
    // not separate elements.
    let mut closers: Vec<char> = Vec::new();
    let mut chars = s.chars().peekable();

    while let Some(c) = chars.next() {
        match c {
            '\\' => {
                cur.push(c);
                if let Some(c2) = chars.next() {
                    cur.push(c2);
                }
            }
            '\'' => {
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
                while let Some(c2) = chars.next() {
                    cur.push(c2);
                    if c2 == '\\' {
                        if let Some(c3) = chars.next() {
                            cur.push(c3);
                        }
                    } else if c2 == c {
                        break;
                    }
                }
            }
            '$' => {
                cur.push(c);
                match chars.peek() {
                    Some(&'(') => {
                        closers.push(')');
                        cur.push('(');
                        chars.next();
                    }
                    Some(&'{') => {
                        closers.push('}');
                        cur.push('{');
                        chars.next();
                    }
                    _ => {}
                }
            }
            '(' if !closers.is_empty() => {
                closers.push(')');
                cur.push(c);
            }
            _ if Some(&c) == closers.last() => {
                closers.pop();
                cur.push(c);
            }
            '#' if closers.is_empty() && cur.is_empty() => {
                while let Some(&c2) = chars.peek() {
                    if c2 == '\n' {
                        break;
                    }
                    chars.next();
                }
            }
            ' ' | '\t' | '\n' | '\r' if closers.is_empty() => {
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
        }
    }

    fn advance(&mut self) {
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

        for line in remaining.lines() {
            let line_len = line.len();
            let trimmed = if strip_tabs {
                line.trim_start_matches('\t')
            } else {
                line
            };

            if trimmed == delimiter {
                resume = (body_start + line_start + line_len + 1).min(input.len());
                found_delimiter = true;
                break;
            }

            if !content.is_empty() {
                content.push('\n');
            }
            content.push_str(trimmed);
            line_start += line_len + 1;
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
        matches!(
            self.current.token,
            Token::RedirectOut
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

    fn parse_redirect(&mut self) -> Result<Redirect, ParseError> {
        let is_heredoc_strip = self.current.token == Token::HereDocStrip;
        let (fd, kind, is_here_doc) = match &self.current.token {
            Token::RedirectOut => (None, RedirectKind::Output, false),
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
                let assign = parse_assignment(&w, self)?;
                assignments.push(assign);
                continue;
            }
            break;
        }

        // Parse words and redirects
        loop {
            if self.is_redirect() {
                redirects.push(self.parse_redirect()?);
            } else if let Token::Word(w) = &self.current.token {
                let w = w.clone();
                words.push(parse_word_parts(&w));
                self.advance();
                // `[[` opens a conditional expression: everything up to the
                // matching `]]` belongs to it, including operator characters.
                if words.len() == 1 && w == "[[" {
                    self.parse_conditional_words(&mut words)?;
                }
            } else {
                break;
            }
        }

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
        }))
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

    fn parse_compound_command(&mut self) -> Result<Command, ParseError> {
        match &self.current.token {
            Token::LParen => {
                // Check if it's (( )) or ( )
                if let Token::LParen = self.peek() {
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
        self.advance(); // consume "for"

        // Check if this is C-style for: for ((
        if let Token::LParen = self.current.token {
            if let Token::LParen = self.peek() {
                return self.parse_c_style_for();
            }
        }

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
            // consume separator
            if self.current.token == Token::Semi || self.current.token == Token::Newline {
                self.advance();
            }
            Some(ws)
        } else {
            if self.current.token == Token::Semi || self.current.token == Token::Newline {
                self.advance();
            }
            None
        };

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
        }))
    }

    fn parse_c_style_for(&mut self) -> Result<Command, ParseError> {
        self.advance(); // consume first (
        self.advance(); // consume second (

        // Slice the raw header text between (( and )) directly from the source.
        // This preserves arithmetic operators (<, >, ++, ...) that the lexer would
        // otherwise split into separate tokens, and handles `;;` (lexed as
        // DoubleSemi) in headers like `for ((;;))`.
        let content_start = self.current.span.0;
        let content_end;
        let mut depth = 0i32;
        loop {
            if self.current.token == Token::Eof {
                return Err(ParseError::Incomplete);
            }
            if self.current.token == Token::RParen && depth == 0 && *self.peek() == Token::RParen {
                content_end = self.current.span.0;
                self.advance(); // consume first )
                self.advance(); // consume second )
                break;
            }
            match self.current.token {
                Token::LParen => depth += 1,
                Token::RParen => depth -= 1,
                _ => {}
            }
            self.advance();
        }

        let raw = self.input.get(content_start..content_end).unwrap_or("");
        let parts = split_for_header(raw);
        let init = parts.get(0).cloned().unwrap_or_default();
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
        self.advance(); // consume "case"
        let word_str = self.expect_word()?;
        let word = parse_word_parts(&word_str);
        self.skip_newlines();
        self.expect_keyword("in")?;
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
        self.advance(); // consume "select"
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
            // consume separator
            if self.current.token == Token::Semi || self.current.token == Token::Newline {
                self.advance();
            }
            Some(ws)
        } else {
            if self.current.token == Token::Semi || self.current.token == Token::Newline {
                self.advance();
            }
            None
        };

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
        }))
    }

    fn parse_arithmetic_command(&mut self) -> Result<Command, ParseError> {
        self.advance(); // consume first (
        self.advance(); // consume second (

        // Read tokens until we find ))
        let mut expr_tokens = Vec::new();
        let mut paren_depth = 0;

        loop {
            match &self.current.token {
                Token::RParen => {
                    if paren_depth == 0 {
                        // Check if next is also RParen
                        if let Token::RParen = self.peek() {
                            self.advance(); // consume first )
                            self.advance(); // consume second )
                            break;
                        } else {
                            paren_depth -= 1;
                            expr_tokens.push(format!("{:?}", self.current.token));
                            self.advance();
                        }
                    } else {
                        paren_depth -= 1;
                        expr_tokens.push(format!("{:?}", self.current.token));
                        self.advance();
                    }
                }
                Token::LParen => {
                    paren_depth += 1;
                    expr_tokens.push(format!("{:?}", self.current.token));
                    self.advance();
                }
                Token::Eof => {
                    return Err(ParseError::Incomplete);
                }
                Token::Word(w) => {
                    expr_tokens.push(w.clone());
                    self.advance();
                }
                Token::Newline => {
                    self.skip_newlines();
                }
                other => {
                    // For operators like &&, ||, |, etc., convert to string
                    expr_tokens.push(match other {
                        Token::And => "&&".to_string(),
                        Token::Or => "||".to_string(),
                        Token::Pipe => "|".to_string(),
                        Token::Semi => ";".to_string(),
                        Token::RedirectOut => ">".to_string(),
                        Token::RedirectAppend => ">>".to_string(),
                        Token::RedirectIn => "<".to_string(),
                        _ => format!("{:?}", other),
                    });
                    self.advance();
                }
            }
        }

        let expr = expr_tokens.join(" ");
        let redirects = self.parse_optional_redirects()?;
        Ok(Command::Compound(CompoundCommand::Arithmetic {
            expr,
            redirects,
        }))
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
                // Parse coproc command
                let w = w.clone();
                if w == "coproc" {
                    self.advance(); // consume "coproc"

                    // Check if next token is a name (word without redirects/pipes)
                    let mut coproc_name = None;

                    // Peek at the next token
                    let next_is_simple = if let Token::Word(potential_name) = &self.current.token {
                        let potential = potential_name.clone();
                        let peek_token = self.peek().clone();
                        if matches!(peek_token, Token::Word(_) | Token::LParen | Token::LBrace) {
                            coproc_name = Some(potential);
                            true
                        } else {
                            false
                        }
                    } else {
                        false
                    };

                    if next_is_simple {
                        self.advance();
                    }

                    // Parse the actual command as simple command
                    let cmd = self.parse_simple_command()?;
                    if let Command::Simple(simple) = cmd {
                        let redirects = self.parse_optional_redirects()?;
                        return Ok(Command::Compound(CompoundCommand::Coproc {
                            name: coproc_name,
                            command: Box::new(simple),
                            redirects,
                        }));
                    } else {
                        return Err(ParseError::Unexpected(
                            "coproc requires a simple command".into(),
                        ));
                    }
                } else {
                    unreachable!()
                }
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

    fn parse_pipeline(&mut self) -> Result<Pipeline, ParseError> {
        let negated = if self.is_keyword("!") {
            self.advance();
            true
        } else {
            false
        };

        let mut commands = vec![self.parse_command()?];

        while self.current.token == Token::Pipe || self.current.token == Token::PipeAnd {
            self.advance();
            self.skip_newlines();
            commands.push(self.parse_command()?);
        }

        Ok(Pipeline { negated, commands })
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

fn parse_assignment(w: &str, parser: &mut Parser) -> Result<Assignment, ParseError> {
    // Detect += vs =
    let (before_eq, value_str, append) = if let Some(pos) = w.find("+=") {
        (&w[..pos], &w[pos + 2..], true)
    } else {
        let eq_pos = w.find('=').unwrap();
        (&w[..eq_pos], &w[eq_pos + 1..], false)
    };

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

/// Parse a raw word string into WordPart components.
/// Crude heuristic to keep `let u = {"name":"bob"}` from being eaten by bash
/// brace expansion: if the contents include a `"` or an unquoted `:`, treat
/// the `{...}` as an opaque literal. Bash brace expansion never legitimately
/// contains a `"`, and `:` only appears in parameter substitution which is
/// inside `${...}`, not `{...}`.
fn looks_like_json_object(content: &str) -> bool {
    content.contains('"') || content.contains(':')
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
            '{' => {
                depth += 1;
                inner.push(c);
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
                            let mut expr = String::new();
                            let mut depth = 1;
                            while let Some(&c2) = chars.peek() {
                                if c2 == ')' {
                                    chars.next();
                                    if chars.peek() == Some(&')') {
                                        chars.next();
                                        depth -= 1;
                                        if depth == 0 {
                                            break;
                                        }
                                    }
                                    expr.push(')');
                                } else if c2 == '(' {
                                    chars.next();
                                    if chars.peek() == Some(&'(') {
                                        chars.next();
                                        depth += 1;
                                        expr.push_str("((");
                                    } else {
                                        expr.push('(');
                                    }
                                } else {
                                    expr.push(c2);
                                    chars.next();
                                }
                            }
                            parts.push(WordPart::Arithmetic(expr));
                        } else {
                            // Command substitution $(...)
                            let cmd = read_command_sub(&mut chars);
                            parts.push(WordPart::CommandSub(cmd));
                        }
                    }
                    Some(&'{') => {
                        chars.next();
                        let mut var = String::new();
                        while let Some(&c2) = chars.peek() {
                            if c2 == '}' {
                                chars.next();
                                break;
                            }
                            var.push(c2);
                            chars.next();
                        }
                        parts.push(WordPart::Variable(var));
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
                            || c2 == '*' =>
                    {
                        let mut var = String::new();
                        if "?$!#@*".contains(c2) {
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
                // Try brace expansion: {a,b,c} or {1..10}
                if !literal.is_empty() {
                    parts.push(WordPart::Literal(std::mem::take(&mut literal)));
                }
                chars.next(); // consume '{'
                let mut content = String::new();
                let mut depth = 1;
                let mut found_close = false;
                let save = chars.clone();
                while let Some(&c2) = chars.peek() {
                    if c2 == '{' {
                        depth += 1;
                    }
                    if c2 == '}' {
                        depth -= 1;
                        if depth == 0 {
                            chars.next();
                            found_close = true;
                            break;
                        }
                    }
                    content.push(c2);
                    chars.next();
                }
                if !found_close {
                    // No closing brace - treat as literal
                    literal.push('{');
                    literal.push_str(&content);
                    chars = save;
                    // Consume all characters we already consumed from save
                    for _ in 0..content.len() {
                        chars.next();
                    }
                    continue;
                }
                // Check if it's a range: start..end[..step]
                if let Some(range) = parse_brace_range(&content) {
                    parts.push(range);
                } else if content.contains(',') && !looks_like_json_object(&content) {
                    // Comma-separated brace expansion: {a,b,c}
                    let items: Vec<Vec<WordPart>> =
                        content.split(',').map(|s| parse_word_parts(s)).collect();
                    parts.push(WordPart::BraceExpansion(items));
                } else {
                    // Not a valid brace expansion (or it's a JSON object) — keep literal.
                    literal.push('{');
                    literal.push_str(&content);
                    literal.push('}');
                }
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
                        || c2 == '*' =>
                {
                    let mut var = String::new();
                    if "?$!#@*".contains(c2) {
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
                    let mut var = String::new();
                    let mut depth = 1;
                    while let Some(&c2) = chars.peek() {
                        if c2 == '{' {
                            depth += 1;
                        }
                        if c2 == '}' {
                            depth -= 1;
                            if depth == 0 {
                                chars.next();
                                break;
                            }
                        }
                        var.push(c2);
                        chars.next();
                    }
                    parts.push(WordPart::Variable(var));
                }
                Some(&'(') => {
                    chars.next();
                    if chars.peek() == Some(&'(') {
                        // Arithmetic $((...))
                        chars.next();
                        let mut expr = String::new();
                        let mut depth = 1;
                        while let Some(&c2) = chars.peek() {
                            if c2 == ')' {
                                chars.next();
                                if chars.peek() == Some(&')') {
                                    chars.next();
                                    depth -= 1;
                                    if depth == 0 {
                                        break;
                                    }
                                    expr.push_str("))");
                                } else {
                                    expr.push(')');
                                }
                            } else if c2 == '(' {
                                chars.next();
                                depth += 1;
                                expr.push('(');
                            } else {
                                expr.push(c2);
                                chars.next();
                            }
                        }
                        parts.push(WordPart::Arithmetic(expr));
                    } else {
                        // Command substitution $(...)
                        let cmd = read_command_sub(&mut chars);
                        parts.push(WordPart::CommandSub(cmd));
                    }
                }
                _ => literal.push('$'),
            }
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

#[cfg(test)]
mod tests {
    use super::{parse, parse_word_parts, WordPart};

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
}

/// Decode ANSI-C escape sequences for $'...' quoting.
fn decode_ansi_c(s: &str) -> String {
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

fn parse_brace_range(content: &str) -> Option<WordPart> {
    let parts: Vec<&str> = content.split("..").collect();
    if parts.len() == 2 {
        Some(WordPart::BraceRange {
            start: parts[0].to_string(),
            end: parts[1].to_string(),
            step: None,
        })
    } else if parts.len() == 3 {
        Some(WordPart::BraceRange {
            start: parts[0].to_string(),
            end: parts[1].to_string(),
            step: Some(parts[2].to_string()),
        })
    } else {
        None
    }
}
