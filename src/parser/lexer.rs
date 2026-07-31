/// Tokenizer for bash-compatible shell syntax.
/// Supports strict mode (for execution) and lenient mode (for highlighting).

#[derive(Debug, Clone, PartialEq)]
pub enum Token {
    Word(String),
    Pipe,              // |
    PipeAnd,           // |&
    And,               // &&
    Or,                // ||
    Semi,              // ;
    Amp,               // &
    AmpBang,           // &!  (background + disown)
    RedirectOut,       // >
    RedirectAppend,    // >>
    RedirectIn,        // <
    HereDoc,           // <<
    HereDocStrip,      // <<-
    HereString,        // <<<
    DupFd,             // >&
    RedirectAllOut,    // &> (redirect stdout and stderr)
    RedirectAllAppend, // &>> (append stdout and stderr)
    RedirectFd(i32, RedirectOp),
    LParen,        // (
    RParen,        // )
    LBrace,        // {   (reserved word)
    RBrace,        // }   (reserved word)
    DoubleSemi,    // ;;
    SemiAmp,       // ;&   (case fall-through)
    DoubleSemiAmp, // ;;&  (case continue-match)
    Newline,
    Eof,
}

#[derive(Debug, Clone, PartialEq)]
pub enum RedirectOp {
    Output,
    Append,
    Input,
    DupOutput, // N>&M  duplicate output fd
    DupInput,  // N<&M  duplicate input fd
}

#[derive(Debug, Clone)]
pub struct SpannedToken {
    pub token: Token,
    pub span: (usize, usize),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum LexicalIssue {
    UnterminatedSingleQuote,
    UnterminatedDoubleQuote,
    UnterminatedAnsiCQuote,
    UnclosedCommandSubstitution,
    UnclosedParameterExpansion,
    UnclosedProcessSubstitution,
    UnclosedExtendedGlob,
    TrailingEscape,
}

pub struct Lexer<'a> {
    input: &'a str,
    pos: usize,
    lenient: bool,
    issue: Option<LexicalIssue>,
    /// The token emitted before the one being read. Only `!(` needs it: at the
    /// start of a command `!` negates, anywhere else it opens an extended glob.
    prev_token: Option<Token>,
}

impl<'a> Lexer<'a> {
    pub fn new(input: &'a str) -> Self {
        Lexer {
            input,
            pos: 0,
            lenient: false,
            issue: None,
            prev_token: None,
        }
    }

    pub fn new_lenient(input: &'a str) -> Self {
        Lexer {
            input,
            pos: 0,
            lenient: true,
            issue: None,
            prev_token: None,
        }
    }

    fn record_issue(&mut self, issue: LexicalIssue) {
        if !self.lenient && self.issue.is_none() {
            self.issue = Some(issue);
        }
    }

    pub(super) fn has_incomplete_construct(&self) -> bool {
        self.issue.is_some()
    }

    fn peek_char(&self) -> Option<char> {
        self.input[self.pos..].chars().next()
    }

    fn next_char(&mut self) -> Option<char> {
        let c = self.peek_char()?;
        self.pos += c.len_utf8();
        Some(c)
    }

    fn peek_char_at(&self, offset: usize) -> Option<char> {
        self.input[self.pos..].chars().nth(offset)
    }

    fn skip_whitespace(&mut self) {
        while let Some(c) = self.peek_char() {
            if c == ' ' || c == '\t' {
                self.next_char();
            } else if c == '\\' && self.peek_char_at(1) == Some('\n') {
                // Treat an unquoted line continuation as whitespace between words.
                self.next_char();
                self.next_char();
            } else if c == '#' {
                // Skip comment to end of line
                while let Some(c) = self.peek_char() {
                    if c == '\n' {
                        break;
                    }
                    self.next_char();
                }
            } else {
                break;
            }
        }
    }

    fn read_single_quoted(&mut self) -> (String, bool) {
        let mut s = String::new();
        loop {
            match self.next_char() {
                Some('\'') => return (s, true),
                Some(c) => s.push(c),
                None => {
                    self.record_issue(LexicalIssue::UnterminatedSingleQuote);
                    return (s, false);
                }
            }
        }
    }

    fn read_ansi_c_quoted(&mut self) -> (String, bool) {
        let mut s = String::new();
        loop {
            match self.next_char() {
                Some('\'') => return (s, true),
                Some('\\') => {
                    s.push('\\');
                    if let Some(next) = self.next_char() {
                        s.push(next);
                    }
                }
                Some(c) => s.push(c),
                None => {
                    self.record_issue(LexicalIssue::UnterminatedAnsiCQuote);
                    return (s, false);
                }
            }
        }
    }

    fn read_backtick_substitution(&mut self) -> (String, bool) {
        let mut s = String::new();
        loop {
            match self.next_char() {
                Some('`') => return (s, true),
                Some('\\') => {
                    s.push('\\');
                    if let Some(next) = self.next_char() {
                        s.push(next);
                    }
                }
                Some(c) => s.push(c),
                None => {
                    self.record_issue(LexicalIssue::UnclosedCommandSubstitution);
                    return (s, false);
                }
            }
        }
    }

    /// Read through a command/process substitution body. Recursive descent keeps
    /// delimiters inside quotes and nested substitutions from closing the outer
    /// construct. Comments are skipped lexically, so punctuation inside them is
    /// never treated as shell syntax.
    fn read_paren_body(&mut self, word: &mut String, issue: LexicalIssue) -> bool {
        let mut at_word_start = true;

        loop {
            let c = match self.next_char() {
                Some(c) => c,
                None => {
                    self.record_issue(issue);
                    return false;
                }
            };

            match c {
                '#' if at_word_start => {
                    word.push(c);
                    loop {
                        match self.next_char() {
                            Some(next) => {
                                word.push(next);
                                if next == '\n' {
                                    at_word_start = true;
                                    break;
                                }
                            }
                            None => {
                                self.record_issue(issue);
                                return false;
                            }
                        }
                    }
                }
                ')' => {
                    word.push(c);
                    return true;
                }
                '\\' => {
                    word.push(c);
                    match self.next_char() {
                        Some(next) => word.push(next),
                        None => {
                            self.record_issue(issue);
                            return false;
                        }
                    }
                    at_word_start = false;
                }
                '\'' => {
                    word.push(c);
                    let (body, closed) = self.read_single_quoted();
                    word.push_str(&body);
                    if !closed {
                        return false;
                    }
                    word.push('\'');
                    at_word_start = false;
                }
                '"' => {
                    word.push(c);
                    let (body, closed) = self.read_double_quoted();
                    word.push_str(&body);
                    if !closed {
                        return false;
                    }
                    word.push('"');
                    at_word_start = false;
                }
                '$' if self.peek_char() == Some('\'') => {
                    self.next_char();
                    word.push('$');
                    word.push('\'');
                    let (body, closed) = self.read_ansi_c_quoted();
                    word.push_str(&body);
                    if !closed {
                        return false;
                    }
                    word.push('\'');
                    at_word_start = false;
                }
                '$' if self.peek_char() == Some('(') => {
                    self.next_char();
                    word.push('$');
                    word.push('(');
                    if !self.read_paren_body(word, LexicalIssue::UnclosedCommandSubstitution) {
                        return false;
                    }
                    at_word_start = false;
                }
                '$' if self.peek_char() == Some('{') => {
                    self.next_char();
                    word.push('$');
                    word.push('{');
                    if !self.read_brace_body(word) {
                        return false;
                    }
                    at_word_start = false;
                }
                '<' | '>' if self.peek_char() == Some('(') => {
                    self.next_char();
                    word.push(c);
                    word.push('(');
                    if !self.read_paren_body(word, LexicalIssue::UnclosedProcessSubstitution) {
                        return false;
                    }
                    at_word_start = false;
                }
                '`' => {
                    word.push(c);
                    let (body, closed) = self.read_backtick_substitution();
                    word.push_str(&body);
                    if !closed {
                        return false;
                    }
                    word.push('`');
                    at_word_start = false;
                }
                '(' => {
                    word.push(c);
                    if !self.read_paren_body(word, issue) {
                        return false;
                    }
                    at_word_start = false;
                }
                c if c.is_whitespace() => {
                    word.push(c);
                    at_word_start = true;
                }
                ';' | '|' | '&' | '<' | '>' => {
                    word.push(c);
                    at_word_start = true;
                }
                _ => {
                    word.push(c);
                    at_word_start = false;
                }
            }
        }
    }

    /// Read the body of an extended glob group — the `(...)` of `@(a|b)` — with
    /// the opening paren already consumed. Unlike a command substitution the
    /// body is a *pattern*: `#` is an ordinary character and `|` separates
    /// alternatives, so only nesting, quoting and expansions may hide a `)`.
    fn read_extglob_body(&mut self, word: &mut String) -> bool {
        let mut depth = 1usize;
        loop {
            let c = match self.next_char() {
                Some(c) => c,
                None => {
                    self.record_issue(LexicalIssue::UnclosedExtendedGlob);
                    return false;
                }
            };
            match c {
                '\\' => {
                    word.push(c);
                    match self.next_char() {
                        Some(next) => word.push(next),
                        None => {
                            self.record_issue(LexicalIssue::UnclosedExtendedGlob);
                            return false;
                        }
                    }
                }
                '\'' => {
                    word.push(c);
                    let (body, closed) = self.read_single_quoted();
                    word.push_str(&body);
                    if !closed {
                        self.record_issue(LexicalIssue::UnterminatedSingleQuote);
                        return false;
                    }
                    word.push('\'');
                }
                '"' => {
                    word.push(c);
                    let (body, closed) = self.read_double_quoted();
                    word.push_str(&body);
                    if !closed {
                        self.record_issue(LexicalIssue::UnterminatedDoubleQuote);
                        return false;
                    }
                    word.push('"');
                }
                '`' => {
                    word.push(c);
                    let (body, closed) = self.read_backtick_substitution();
                    word.push_str(&body);
                    if !closed {
                        self.record_issue(LexicalIssue::UnclosedCommandSubstitution);
                        return false;
                    }
                    word.push('`');
                }
                '$' if self.peek_char() == Some('(') => {
                    self.next_char();
                    word.push('$');
                    word.push('(');
                    if !self.read_paren_body(word, LexicalIssue::UnclosedCommandSubstitution) {
                        return false;
                    }
                }
                '$' if self.peek_char() == Some('{') => {
                    self.next_char();
                    word.push('$');
                    word.push('{');
                    if !self.read_brace_body(word) {
                        return false;
                    }
                }
                '(' => {
                    depth += 1;
                    word.push(c);
                }
                ')' => {
                    word.push(c);
                    depth -= 1;
                    if depth == 0 {
                        return true;
                    }
                }
                _ => word.push(c),
            }
        }
    }

    /// True when the `(` about to be read opens an extended glob group, i.e.
    /// `word` ends in an unquoted, unescaped `?`, `*`, `+`, `@` or `!`.
    ///
    /// A leading `!` is the exception: `!(cmd)` at the start of a command is
    /// bash's pipeline negation, and only `shopt -s extglob` — which the lexer
    /// cannot see — makes bash read it as a pattern instead. Keeping negation
    /// wherever a command may start is the reading that cannot break a working
    /// script, since every other position rejects `!` as a word anyway.
    fn opens_extglob(&self, word: &str) -> bool {
        let prefix = match word.chars().last() {
            Some(c @ ('?' | '*' | '+' | '@' | '!')) => c,
            _ => return false,
        };
        // `\@(` and `'@'(` are literal: an escaped prefix is preceded by an odd
        // number of backslashes, and a quoted one ends the word with a quote.
        let before = &word[..word.len() - prefix.len_utf8()];
        if before.chars().rev().take_while(|&c| c == '\\').count() % 2 == 1 {
            return false;
        }
        prefix != '!' || !before.is_empty() || !self.at_command_start()
    }

    /// True when the next word would start a command, the one place `!` is a
    /// reserved word rather than the head of a pattern.
    fn at_command_start(&self) -> bool {
        match &self.prev_token {
            None => true,
            Some(Token::Word(w)) => matches!(
                w.as_str(),
                "!" | "if" | "then" | "elif" | "else" | "while" | "until" | "do"
            ),
            Some(token) => matches!(
                token,
                Token::Newline
                    | Token::Semi
                    | Token::Amp
                    | Token::AmpBang
                    | Token::And
                    | Token::Or
                    | Token::Pipe
                    | Token::PipeAnd
                    | Token::LParen
                    | Token::LBrace
                    | Token::DoubleSemi
                    | Token::SemiAmp
                    | Token::DoubleSemiAmp
            ),
        }
    }

    fn read_dollar_paren(&mut self, word: &mut String) {
        self.next_char(); // $
        self.next_char(); // (
        word.push('$');
        word.push('(');
        self.read_paren_body(word, LexicalIssue::UnclosedCommandSubstitution);
    }

    fn read_brace_body(&mut self, word: &mut String) -> bool {
        loop {
            let c = match self.next_char() {
                Some(c) => c,
                None => {
                    self.record_issue(LexicalIssue::UnclosedParameterExpansion);
                    return false;
                }
            };

            match c {
                '}' => {
                    word.push(c);
                    return true;
                }
                '\\' => {
                    word.push(c);
                    match self.next_char() {
                        Some(next) => word.push(next),
                        None => {
                            self.record_issue(LexicalIssue::UnclosedParameterExpansion);
                            return false;
                        }
                    }
                }
                '\'' => {
                    word.push(c);
                    let (body, closed) = self.read_single_quoted();
                    word.push_str(&body);
                    if !closed {
                        return false;
                    }
                    word.push('\'');
                }
                '"' => {
                    word.push(c);
                    let (body, closed) = self.read_double_quoted();
                    word.push_str(&body);
                    if !closed {
                        return false;
                    }
                    word.push('"');
                }
                '$' if self.peek_char() == Some('\'') => {
                    self.next_char();
                    word.push('$');
                    word.push('\'');
                    let (body, closed) = self.read_ansi_c_quoted();
                    word.push_str(&body);
                    if !closed {
                        return false;
                    }
                    word.push('\'');
                }
                '$' if self.peek_char() == Some('(') => {
                    self.next_char();
                    word.push('$');
                    word.push('(');
                    if !self.read_paren_body(word, LexicalIssue::UnclosedCommandSubstitution) {
                        return false;
                    }
                }
                '$' if self.peek_char() == Some('{') => {
                    self.next_char();
                    word.push('$');
                    word.push('{');
                    if !self.read_brace_body(word) {
                        return false;
                    }
                }
                '<' | '>' if self.peek_char() == Some('(') => {
                    self.next_char();
                    word.push(c);
                    word.push('(');
                    if !self.read_paren_body(word, LexicalIssue::UnclosedProcessSubstitution) {
                        return false;
                    }
                }
                '`' => {
                    word.push(c);
                    let (body, closed) = self.read_backtick_substitution();
                    word.push_str(&body);
                    if !closed {
                        return false;
                    }
                    word.push('`');
                }
                // A bare `{` does not nest: only `${` opens a new expansion.
                // Treating it as an opener swallowed the closing brace of
                // patterns that merely contain one, as in
                // `${opt%%[<{().[]*}`, and the rest of the script with it.
                _ => word.push(c),
            }
        }
    }

    fn read_dollar_brace(&mut self, word: &mut String) {
        self.next_char(); // $
        self.next_char(); // {
        word.push('$');
        word.push('{');
        self.read_brace_body(word);
    }

    fn read_double_quoted(&mut self) -> (String, bool) {
        // Preserve backslash escapes verbatim so the parser's word-part stage can
        // decide how to handle them (a backslash only escapes $ ` " \ newline inside
        // double quotes). Backslash-newline is a line continuation and is removed.
        let mut s = String::new();
        loop {
            match self.next_char() {
                Some('"') => return (s, true),
                Some('$') if self.peek_char() == Some('(') => {
                    s.push('$');
                    s.push('(');
                    self.next_char(); // consume '(' after $
                    if !self.read_paren_body(&mut s, LexicalIssue::UnclosedCommandSubstitution) {
                        return (s, false);
                    }
                }
                Some('$') if self.peek_char() == Some('{') => {
                    s.push('$');
                    self.next_char(); // consume {
                    s.push('{');
                    if !self.read_brace_body(&mut s) {
                        return (s, false);
                    }
                }
                Some('`') => {
                    s.push('`');
                    let (body, closed) = self.read_backtick_substitution();
                    s.push_str(&body);
                    if !closed {
                        return (s, false);
                    }
                    s.push('`');
                }
                Some('\\') => {
                    match self.next_char() {
                        Some('\n') => {} // line continuation
                        Some(c) => {
                            s.push('\\');
                            s.push(c);
                        }
                        None => {
                            s.push('\\');
                            self.record_issue(LexicalIssue::UnterminatedDoubleQuote);
                            return (s, false);
                        }
                    }
                }
                Some(c) => s.push(c),
                None => {
                    self.record_issue(LexicalIssue::UnterminatedDoubleQuote);
                    return (s, false);
                }
            }
        }
    }

    fn read_word(&mut self) -> String {
        let mut word = String::new();
        // Closure literal `{|params| body}` — read greedily until matching `}`
        // so word splitting on whitespace inside the body does not corrupt it.
        if self.peek_char() == Some('{') && self.peek_char_at(1) == Some('|') {
            self.next_char(); // {
            self.next_char(); // |
            word.push('{');
            word.push('|');
            let mut depth: i32 = 1;
            let mut in_single = false;
            let mut in_double = false;
            while let Some(c) = self.next_char() {
                word.push(c);
                if c == '\\' {
                    if let Some(nc) = self.next_char() {
                        word.push(nc);
                    }
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
                if c == '{' {
                    depth += 1;
                } else if c == '}' {
                    depth -= 1;
                    if depth == 0 {
                        break;
                    }
                }
            }
            return word;
        }
        loop {
            match self.peek_char() {
                None => break,
                Some(c) => match c {
                    ' ' | '\t' | '\n' | '|' | '&' | ';' => break,
                    '(' | ')' => {
                        // Extended glob: `@(a|b)`, `?(x)`, `*(x)`, `+(x)`, `!(x)`.
                        // The group belongs to the word, so a pattern keeps its
                        // shape whether it lands in `[[ ]]`, a `case` label or a
                        // plain argument.
                        if c == '(' && self.opens_extglob(&word) {
                            self.next_char();
                            word.push('(');
                            self.read_extglob_body(&mut word);
                        }
                        // Check for array assignment: name=(...)
                        // If word contains '=' and we see '(', continue reading the array
                        else if c == '(' && word.contains('=') {
                            // This is array assignment, read until matching )
                            self.next_char();
                            word.push('(');
                            let mut paren_depth = 1;
                            while let Some(c2) = self.peek_char() {
                                if c2 == '(' {
                                    paren_depth += 1;
                                } else if c2 == ')' {
                                    paren_depth -= 1;
                                    if paren_depth == 0 {
                                        self.next_char();
                                        word.push(')');
                                        break;
                                    }
                                }
                                self.next_char();
                                word.push(c2);
                            }
                        } else {
                            break;
                        }
                    }
                    '<' | '>' => {
                        // Check for process substitution <(...) or >(...)
                        if self.peek_char_at(1) == Some('(') {
                            self.next_char(); // consume < or >
                            self.next_char(); // consume (
                            word.push(c);
                            word.push('(');
                            self.read_paren_body(
                                &mut word,
                                LexicalIssue::UnclosedProcessSubstitution,
                            );
                        } else {
                            break; // normal redirect
                        }
                    }
                    '$' if self.peek_char_at(1) == Some('\'') => {
                        // ANSI-C quoting $'...' -- preserve backslash escapes so the
                        // parser can decode them; a backslash-escaped ' does not close.
                        self.next_char(); // $
                        self.next_char(); // '
                        word.push('$');
                        word.push('\'');
                        let (body, closed) = self.read_ansi_c_quoted();
                        word.push_str(&body);
                        if closed {
                            word.push('\'');
                        }
                    }
                    '$' if self.peek_char_at(1) == Some('(') => {
                        // Command substitution $(...) or arithmetic $((...)).
                        // Keep the whole raw construct in one word, respecting
                        // nested quotes and nested $() so spaces inside do not
                        // terminate the surrounding token.
                        self.read_dollar_paren(&mut word);
                    }
                    '$' if self.peek_char_at(1) == Some('{') => {
                        self.read_dollar_brace(&mut word);
                    }
                    '\'' => {
                        self.next_char();
                        word.push('\'');
                        let (s, _closed) = self.read_single_quoted();
                        word.push_str(&s);
                        // Preserve the historical lenient token shape. Strict mode
                        // rejects a synthesized close via `issue` before execution.
                        word.push('\'');
                    }
                    '"' => {
                        self.next_char();
                        word.push('"');
                        let (s, _closed) = self.read_double_quoted();
                        word.push_str(&s);
                        // See the single-quote case above.
                        word.push('"');
                    }
                    '`' => {
                        self.next_char();
                        word.push('`');
                        let (body, closed) = self.read_backtick_substitution();
                        word.push_str(&body);
                        if closed {
                            word.push('`');
                        }
                    }
                    '\\' => {
                        self.next_char();
                        if self.peek_char() == Some('\n') {
                            // An unquoted backslash-newline is a line continuation,
                            // not part of a word (POSIX shell grammar).
                            self.next_char();
                        } else {
                            word.push('\\');
                            if let Some(c2) = self.next_char() {
                                word.push(c2);
                            } else {
                                self.record_issue(LexicalIssue::TrailingEscape);
                            }
                        }
                    }
                    '#' if word.is_empty() => break, // comment
                    _ => {
                        self.next_char();
                        word.push(c);
                    }
                },
            }
        }
        word
    }

    pub fn next_token(&mut self) -> SpannedToken {
        self.skip_whitespace();
        let start = self.pos;

        let token = match self.peek_char() {
            None => Token::Eof,
            Some('\n') => {
                self.next_char();
                Token::Newline
            }
            Some('|') => {
                self.next_char();
                match self.peek_char() {
                    Some('|') => {
                        self.next_char();
                        Token::Or
                    }
                    Some('&') => {
                        self.next_char();
                        Token::PipeAnd
                    }
                    _ => Token::Pipe,
                }
            }
            Some('&') => {
                self.next_char();
                match self.peek_char() {
                    Some('&') => {
                        self.next_char();
                        Token::And
                    }
                    Some('!') => {
                        self.next_char();
                        Token::AmpBang
                    }
                    Some('>') => {
                        self.next_char();
                        match self.peek_char() {
                            Some('>') => {
                                self.next_char();
                                Token::RedirectAllAppend
                            }
                            _ => Token::RedirectAllOut,
                        }
                    }
                    _ => Token::Amp,
                }
            }
            Some(';') => {
                self.next_char();
                match self.peek_char() {
                    Some(';') => {
                        self.next_char();
                        match self.peek_char() {
                            Some('&') => {
                                self.next_char();
                                Token::DoubleSemiAmp
                            }
                            _ => Token::DoubleSemi,
                        }
                    }
                    Some('&') => {
                        self.next_char();
                        Token::SemiAmp
                    }
                    _ => Token::Semi,
                }
            }
            Some('(') => {
                self.next_char();
                Token::LParen
            }
            Some(')') => {
                self.next_char();
                Token::RParen
            }
            Some('>') => {
                self.next_char();
                match self.peek_char() {
                    Some('>') => {
                        self.next_char();
                        Token::RedirectAppend
                    }
                    Some('&') => {
                        self.next_char();
                        Token::DupFd
                    }
                    Some('(') => {
                        // Process substitution >(cmd) -- back up and read as word
                        self.pos = start;
                        let w = self.read_word();
                        Token::Word(w)
                    }
                    _ => Token::RedirectOut,
                }
            }
            Some('<') => {
                self.next_char();
                match self.peek_char() {
                    Some('<') => {
                        self.next_char();
                        match self.peek_char() {
                            Some('<') => {
                                self.next_char();
                                Token::HereString
                            }
                            Some('-') => {
                                self.next_char();
                                Token::HereDocStrip
                            }
                            _ => Token::HereDoc,
                        }
                    }
                    Some('(') => {
                        // Process substitution <(cmd) -- back up and read as word
                        self.pos = start;
                        let w = self.read_word();
                        Token::Word(w)
                    }
                    _ => Token::RedirectIn,
                }
            }
            Some(c) if c.is_ascii_digit() => {
                // Check if it's a redirect like 2> or 2>>
                let saved_pos = self.pos;
                let mut num_str = String::new();
                while let Some(d) = self.peek_char() {
                    if d.is_ascii_digit() {
                        num_str.push(d);
                        self.next_char();
                    } else {
                        break;
                    }
                }
                match self.peek_char() {
                    Some('>') => {
                        let fd: i32 = num_str.parse().unwrap_or(1);
                        self.next_char();
                        match self.peek_char() {
                            Some('>') => {
                                self.next_char();
                                Token::RedirectFd(fd, RedirectOp::Append)
                            }
                            Some('&') => {
                                self.next_char();
                                Token::RedirectFd(fd, RedirectOp::DupOutput)
                            }
                            _ => Token::RedirectFd(fd, RedirectOp::Output),
                        }
                    }
                    Some('<') => {
                        let fd: i32 = num_str.parse().unwrap_or(0);
                        self.next_char();
                        match self.peek_char() {
                            Some('&') => {
                                self.next_char();
                                Token::RedirectFd(fd, RedirectOp::DupInput)
                            }
                            _ => Token::RedirectFd(fd, RedirectOp::Input),
                        }
                    }
                    _ => {
                        // Not a redirect, read as word
                        self.pos = saved_pos;
                        let w = self.read_word();
                        if w == "{" {
                            Token::LBrace
                        } else if w == "}" {
                            Token::RBrace
                        } else {
                            Token::Word(w)
                        }
                    }
                }
            }
            _ => {
                let w = self.read_word();
                if w.is_empty() {
                    // Shouldn't happen but safety
                    self.next_char();
                    Token::Eof
                } else if w == "{" {
                    Token::LBrace
                } else if w == "}" {
                    Token::RBrace
                } else {
                    Token::Word(w)
                }
            }
        };

        self.prev_token = Some(token.clone());
        SpannedToken {
            token,
            span: (start, self.pos),
        }
    }

    pub fn tokenize_all(&mut self) -> Vec<SpannedToken> {
        let mut tokens = Vec::new();
        loop {
            let t = self.next_token();
            if t.token == Token::Eof {
                tokens.push(t);
                break;
            }
            tokens.push(t);
        }
        tokens
    }

    /// Get the current position in the input stream
    pub fn pos(&self) -> usize {
        self.pos
    }

    /// Get the remaining input from current position
    pub fn remaining_input(&self) -> &'a str {
        &self.input[self.pos..]
    }

    /// Set position (used for advancing past here-doc content)
    pub fn set_pos(&mut self, pos: usize) {
        self.pos = pos;
    }
}

pub fn tokenize(input: &str) -> Vec<SpannedToken> {
    Lexer::new(input).tokenize_all()
}

pub fn tokenize_lenient(input: &str) -> Vec<SpannedToken> {
    Lexer::new_lenient(input).tokenize_all()
}

#[cfg(test)]
mod tests {
    use super::{tokenize, Lexer, Token};

    #[test]
    fn assignment_with_command_substitution_stays_one_word() {
        let tokens = tokenize(r#"x="$(printf "export A=1\nexport B=2\n")""#);
        assert_eq!(tokens.len(), 2);
        assert_eq!(
            tokens[0].token,
            Token::Word(r#"x="$(printf "export A=1\nexport B=2\n")""#.to_string())
        );
        assert_eq!(tokens[1].token, Token::Eof);
    }

    #[test]
    fn double_quoted_parameter_expansion_keeps_following_lines_separate() {
        let tokens = tokenize(
            "PATH=\"$(\\dirname \"$(\\dirname \"$D\")\")/condabin${PATH:+\":${PATH}\"}\"\n\
             echo done\n",
        );
        assert_eq!(tokens.len(), 6, "{tokens:#?}");
        assert_eq!(
            tokens[0].token,
            Token::Word(
                "PATH=\"$(\\dirname \"$(\\dirname \"$D\")\")/condabin${PATH:+\":${PATH}\"}\""
                    .to_string()
            )
        );
        assert_eq!(tokens[1].token, Token::Newline, "{tokens:#?}");
        assert_eq!(
            tokens[2].token,
            Token::Word("echo".to_string()),
            "{tokens:#?}"
        );
        assert_eq!(
            tokens[3].token,
            Token::Word("done".to_string()),
            "{tokens:#?}"
        );
        assert_eq!(tokens[4].token, Token::Newline, "{tokens:#?}");
        assert_eq!(tokens[5].token, Token::Eof, "{tokens:#?}");
    }

    #[test]
    fn strict_lexer_records_unclosed_shell_constructs() {
        for input in [
            "echo 'unterminated",
            "echo \"unterminated",
            "echo $'unterminated",
            "echo $(printf hi",
            "echo ${value:-fallback",
            "echo <(printf hi",
            "echo >(printf hi",
            "echo `printf hi",
            "echo trailing\\",
        ] {
            let mut lexer = Lexer::new(input);
            lexer.tokenize_all();
            assert!(
                lexer.has_incomplete_construct(),
                "strict lexer accepted {input:?}"
            );
        }
    }

    #[test]
    fn lenient_lexer_keeps_tokens_and_spans_for_unclosed_quotes() {
        let input = "echo 'unterminated";
        let mut lexer = Lexer::new_lenient(input);
        let tokens = lexer.tokenize_all();

        assert_eq!(tokens[0].token, Token::Word("echo".to_string()));
        assert_eq!(tokens[1].token, Token::Word("'unterminated'".to_string()));
        assert_eq!(tokens[1].span, (5, input.len()));
        assert_eq!(&input[tokens[1].span.0..tokens[1].span.1], "'unterminated");
        assert!(!lexer.has_incomplete_construct());
    }
}
