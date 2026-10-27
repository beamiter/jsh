//! AST → shell source, in the layout bash itself uses for `declare -f` and
//! `type NAME`.
//!
//! The printer mirrors bash's `print_cmd.c` rather than inventing a style, so a
//! function defined in jsh describes itself exactly as bash would describe the
//! same definition: four-space indentation, `;` after every command of an
//! `if`/loop body, `elif` spelled as a nested `if`, `for x` spelled as
//! `for x in "$@"`, here-document bodies after the command line, and so on.
//!
//! jsh's AST does not keep the source text of a word, only its parts, so words
//! are re-quoted: unquoted text gets a backslash before each character the
//! lexer or the word parser would otherwise treat specially. Every construct is
//! printed so that parsing the output yields an equivalent AST, and printing
//! that AST again yields the same text (see the round-trip tests below).

use super::ast::{
    Assignment, CaseArm, CaseTerminator, Command, CompleteCommand, CompoundCommand, Connector,
    InterpPart, PathSeg, Pipeline, ProcessSubKind, Redirect, RedirectKind, SimpleCommand,
    TimeFormat, Word, WordPart,
};

const INDENT_STEP: usize = 4;

/// Bash's canonical text of a function definition, without a trailing newline:
///
/// ```text
/// f ()
/// {
///     echo hi
/// }
/// ```
///
/// (Bash leaves a space after `()` and after `{`.) A body that is not a brace
/// group is printed inside braces, as bash does; redirections attached to a
/// brace-group body follow the closing brace.
pub fn format_function(name: &str, body: &CompoundCommand) -> String {
    let mut printer = Printer::default();
    if is_reserved_word(name) {
        printer.out.push_str("function ");
    }
    printer.out.push_str(name);
    printer.out.push_str(" () \n");
    printer.function_body(body);
    printer.out
}

#[derive(Default)]
struct Printer {
    out: String,
    indent: usize,
    /// Here-document bodies whose `<<WORD` header is already printed. Bash
    /// emits them once the command line (including a following connector) is
    /// complete, so they never split a command's own text.
    pending_heredocs: Vec<(String, String)>,
}

impl Printer {
    fn write_indent(&mut self) {
        for _ in 0..self.indent {
            self.out.push(' ');
        }
    }

    /// Bash's `newline()`: a line break, the current indentation, then text.
    fn newline(&mut self, text: &str) {
        self.out.push('\n');
        self.write_indent();
        self.out.push_str(text);
    }

    /// Bash's `semicolon()`: terminate the command just printed, unless it
    /// already ended a line (a here-document body) or was backgrounded.
    fn semicolon(&mut self) {
        if !self.out.ends_with(['&', '\n']) {
            self.out.push(';');
        }
    }

    fn flush_heredocs(&mut self) {
        if self.pending_heredocs.is_empty() {
            return;
        }
        self.out.push('\n');
        for (content, delimiter) in std::mem::take(&mut self.pending_heredocs) {
            self.out.push_str(&content);
            self.out.push_str(&delimiter);
            self.out.push('\n');
        }
    }

    /// `{ ... }` for a function definition; the caller has positioned the
    /// cursor (and indentation) where the opening brace belongs.
    fn function_body(&mut self, body: &CompoundCommand) {
        self.out.push_str("{ \n");
        self.indent += INDENT_STEP;
        self.write_indent();
        let redirects: &[Redirect] = match body {
            CompoundCommand::BraceGroup { body, redirects } => {
                self.list(body);
                redirects
            }
            other => {
                self.compound(other);
                self.flush_heredocs();
                &[]
            }
        };
        self.indent -= INDENT_STEP;
        self.newline("}");
        if !redirects.is_empty() {
            self.out.push(' ');
            self.redirects(redirects);
            self.flush_heredocs();
        }
    }

    /// A command list. The first command starts at the cursor; each following
    /// one starts on a new, indented line unless the previous one was put in
    /// the background, which bash continues on the same line (`a & b`).
    fn list(&mut self, commands: &[CompleteCommand]) {
        for (index, command) in commands.iter().enumerate() {
            let last = index + 1 == commands.len();
            self.and_or(command);
            if command.background {
                self.out.push_str(if command.disown { " &!" } else { " &" });
                if !self.pending_heredocs.is_empty() {
                    self.flush_heredocs();
                    self.out.push(' ');
                }
                if !last {
                    self.out.push(' ');
                }
            } else if !last {
                // A pending here-document body ends the line itself, and bash
                // then drops the `;`.
                if self.pending_heredocs.is_empty() {
                    self.out.push(';');
                } else {
                    self.flush_heredocs();
                }
                self.newline("");
            } else {
                self.flush_heredocs();
            }
        }
    }

    fn and_or(&mut self, command: &CompleteCommand) {
        self.pipeline(&command.list.first);
        for (connector, pipeline) in &command.list.rest {
            self.out.push_str(match connector {
                Connector::And => " && ",
                Connector::Or => " || ",
            });
            if !self.pending_heredocs.is_empty() {
                self.flush_heredocs();
                self.out.push(' ');
            }
            self.pipeline(pipeline);
        }
    }

    fn pipeline(&mut self, pipeline: &Pipeline) {
        // Bash prints the `time` reserved word before any `!`.
        match pipeline.timed {
            Some(TimeFormat::Shell) => self.out.push_str("time "),
            Some(TimeFormat::Posix) => self.out.push_str("time -p "),
            None => {}
        }
        if pipeline.negated {
            self.out.push_str("! ");
        }
        for (index, command) in pipeline.commands.iter().enumerate() {
            if index > 0 {
                self.out.push_str(" |");
                if !self.pending_heredocs.is_empty() {
                    self.flush_heredocs();
                    self.out.push(' ');
                }
                self.out.push(' ');
            }
            self.command(command);
        }
    }

    fn command(&mut self, command: &Command) {
        match command {
            Command::Simple(simple) => self.simple(simple),
            Command::Compound(compound) => self.compound(compound),
            Command::FunctionDef { name, body } => {
                // Bash spells a nested definition with the `function` keyword.
                self.out.push_str("function ");
                self.out.push_str(name);
                self.out.push_str(" () \n");
                self.write_indent();
                self.function_body(body);
            }
        }
    }

    fn compound(&mut self, compound: &CompoundCommand) {
        let redirects: &[Redirect] = match compound {
            CompoundCommand::BraceGroup { body, redirects } => {
                self.out.push_str("{ \n");
                self.indent += INDENT_STEP;
                self.write_indent();
                self.list(body);
                self.indent -= INDENT_STEP;
                self.newline("}");
                redirects
            }
            CompoundCommand::Subshell { body, redirects } => {
                self.out.push_str("( ");
                self.list(body);
                self.out.push_str(" )");
                redirects
            }
            CompoundCommand::If {
                conditions,
                else_branch,
                redirects,
            } => {
                self.if_chain(conditions, else_branch.as_deref());
                redirects
            }
            CompoundCommand::For {
                var,
                words,
                body,
                redirects,
                ..
            } => {
                self.for_head("for", var, words.as_deref());
                self.loop_body(body);
                redirects
            }
            CompoundCommand::Select {
                var,
                words,
                body,
                redirects,
                ..
            } => {
                self.for_head("select", var, words.as_deref());
                self.loop_body(body);
                redirects
            }
            CompoundCommand::CStyleFor {
                init,
                condition,
                update,
                body,
                redirects,
            } => {
                // Bash stores an omitted expression as `1`.
                let or_one = |text: &str| {
                    if text.trim().is_empty() {
                        "1".to_string()
                    } else {
                        text.to_string()
                    }
                };
                self.out.push_str(&format!(
                    "for (({}; {}; {}))",
                    or_one(init),
                    or_one(condition),
                    or_one(update)
                ));
                self.loop_body(body);
                redirects
            }
            CompoundCommand::While {
                condition,
                body,
                redirects,
            } => {
                self.while_loop("while", condition, body);
                redirects
            }
            CompoundCommand::Until {
                condition,
                body,
                redirects,
            } => {
                self.while_loop("until", condition, body);
                redirects
            }
            CompoundCommand::Case {
                word,
                arms,
                redirects,
                ..
            } => {
                self.case(word, arms);
                redirects
            }
            CompoundCommand::Arithmetic { expr, redirects } => {
                self.out.push_str("(( ");
                self.out.push_str(expr.trim());
                self.out.push_str(" ))");
                redirects
            }
            CompoundCommand::Coproc {
                name,
                command,
                redirects,
            } => {
                // Bash names an anonymous coprocess COPROC when printing it.
                // A name only binds before a compound command, though: bash's
                // `coproc COPROC cat` for `coproc cat` re-evaluates as a
                // command named COPROC. Print a simple command unnamed so
                // `eval "$(declare -f f)"` recreates the same function.
                self.out.push_str("coproc ");
                if matches!(**command, Command::Compound(_)) {
                    self.out.push_str(name.as_deref().unwrap_or("COPROC"));
                    self.out.push(' ');
                }
                self.command(command);
                redirects
            }
        };
        if !redirects.is_empty() {
            self.out.push(' ');
            self.redirects(redirects);
        }
    }

    /// `if`/`elif`/`else`, with each `elif` printed as an `if` nested in the
    /// `else` branch, as bash prints it.
    fn if_chain(
        &mut self,
        conditions: &[(Vec<CompleteCommand>, Vec<CompleteCommand>)],
        else_branch: Option<&[CompleteCommand]>,
    ) {
        let Some(((condition, body), rest)) = conditions.split_first() else {
            return;
        };
        self.out.push_str("if ");
        self.list(condition);
        self.semicolon();
        self.out.push_str(" then\n");
        self.indent += INDENT_STEP;
        self.write_indent();
        self.list(body);
        self.indent -= INDENT_STEP;
        if !rest.is_empty() || else_branch.is_some() {
            self.semicolon();
            self.newline("else\n");
            self.indent += INDENT_STEP;
            self.write_indent();
            if rest.is_empty() {
                self.list(else_branch.unwrap_or_default());
            } else {
                self.if_chain(rest, else_branch);
            }
            self.indent -= INDENT_STEP;
        }
        self.semicolon();
        self.newline("fi");
    }

    fn for_head(&mut self, keyword: &str, var: &str, words: Option<&[Word]>) {
        self.out.push_str(keyword);
        self.out.push(' ');
        self.out.push_str(var);
        self.out.push_str(" in ");
        match words {
            Some(words) => {
                let printed: Vec<String> = words.iter().map(|w| unquoted_word(w, false)).collect();
                self.out.push_str(&printed.join(" "));
            }
            // `for x; do` iterates over the positional parameters.
            None => self.out.push_str("\"$@\""),
        }
        self.out.push(';');
    }

    /// `do ... done` for `for`, `select` and arithmetic `for`, which bash puts
    /// on its own line after the loop head.
    fn loop_body(&mut self, body: &[CompleteCommand]) {
        self.newline("do\n");
        self.indent += INDENT_STEP;
        self.write_indent();
        self.list(body);
        self.semicolon();
        self.indent -= INDENT_STEP;
        self.newline("done");
    }

    fn while_loop(
        &mut self,
        keyword: &str,
        condition: &[CompleteCommand],
        body: &[CompleteCommand],
    ) {
        self.out.push_str(keyword);
        self.out.push(' ');
        self.list(condition);
        self.semicolon();
        self.out.push_str(" do\n");
        self.indent += INDENT_STEP;
        self.write_indent();
        self.list(body);
        self.indent -= INDENT_STEP;
        self.semicolon();
        self.newline("done");
    }

    fn case(&mut self, word: &Word, arms: &[CaseArm]) {
        self.out.push_str("case ");
        self.out.push_str(&unquoted_word(word, false));
        self.out.push_str(" in ");
        self.indent += INDENT_STEP;
        for arm in arms {
            self.newline("");
            let patterns: Vec<String> = arm
                .patterns
                .iter()
                .map(|pattern| unquoted_word(pattern, false))
                .collect();
            self.out.push_str(&patterns.join(" | "));
            self.out.push_str(")\n");
            self.indent += INDENT_STEP;
            // Bash prints nothing at all, not even indentation, for an empty arm.
            if !arm.body.is_empty() {
                self.write_indent();
                self.list(&arm.body);
            }
            self.indent -= INDENT_STEP;
            self.newline(match arm.terminator {
                CaseTerminator::Break => ";;",
                CaseTerminator::FallThrough => ";&",
                CaseTerminator::ContinueMatch => ";;&",
            });
        }
        self.indent -= INDENT_STEP;
        self.newline("esac");
    }

    fn simple(&mut self, command: &SimpleCommand) {
        let mut pieces: Vec<String> = command.assignments.iter().map(assignment).collect();
        let conditional = command
            .words
            .first()
            .is_some_and(|word| matches!(word.as_slice(), [WordPart::Literal(w) | WordPart::Glob(w)] if w == "[["));
        let mut previous_was_regex_op = false;
        for (index, word) in command.words.iter().enumerate() {
            let command_position = index == 0 && command.assignments.is_empty();
            let printed = if conditional && index > 0 {
                let printed = if previous_was_regex_op {
                    regex_operand(word)
                } else if let Some(op) = conditional_operator(word) {
                    op.to_string()
                } else {
                    unquoted_word(word, false)
                };
                previous_was_regex_op =
                    matches!(word.as_slice(), [WordPart::Literal(op)] if op == "=~");
                printed
            } else if let Some((_, source)) =
                command.compound_args.iter().find(|(at, _)| *at == index)
            {
                // `declare -A m=([k]=v)`: a declaration's compound assignment
                // keeps its source text, which bash also prints verbatim.
                source.clone()
            } else {
                unquoted_word(word, command_position)
            };
            pieces.push(printed);
        }
        self.out.push_str(&pieces.join(" "));
        if !command.redirects.is_empty() {
            if !pieces.is_empty() {
                self.out.push(' ');
            }
            self.redirects(&command.redirects);
        }
    }

    fn redirects(&mut self, redirects: &[Redirect]) {
        for (index, redirect) in redirects.iter().enumerate() {
            if index > 0 {
                self.out.push(' ');
            }
            self.redirect(redirect);
        }
    }

    fn redirect(&mut self, redirect: &Redirect) {
        let target = || unquoted_word(&redirect.target, false);
        // A descriptor is printed only when it differs from the operator's
        // default, except for duplications, which bash always spells in full.
        // `{name}>file` names a variable that receives (or, for `>&-`,
        // supplies) the descriptor; bash prints it in place of the number.
        let named = redirect.fd_var.as_ref().map(|name| format!("{{{name}}}"));
        let fd_prefix = |default: i32| match (&named, redirect.fd) {
            (Some(named), _) => named.clone(),
            (None, Some(fd)) if fd != default => fd.to_string(),
            _ => String::new(),
        };
        let text = match redirect.kind {
            RedirectKind::Output => format!("{}> {}", fd_prefix(1), target()),
            RedirectKind::Clobber => format!("{}>| {}", fd_prefix(1), target()),
            RedirectKind::Append => format!("{}>> {}", fd_prefix(1), target()),
            RedirectKind::Input => format!("{}< {}", fd_prefix(0), target()),
            RedirectKind::OutputAll => format!("&> {}", target()),
            RedirectKind::AppendAll => format!("&>> {}", target()),
            RedirectKind::HereString => format!("{}<<< {}", fd_prefix(0), target()),
            RedirectKind::DupOutput | RedirectKind::DupInput => {
                let (default, op) = if redirect.kind == RedirectKind::DupOutput {
                    (1, ">&")
                } else {
                    (0, "<&")
                };
                let fd = named
                    .clone()
                    .unwrap_or_else(|| redirect.fd.unwrap_or(default).to_string());
                let target = target();
                if target == "-" || target.bytes().all(|b| b.is_ascii_digit()) {
                    format!("{fd}{op}{target}")
                } else if named.is_none() && redirect.fd.unwrap_or(default) == default {
                    format!("{op}{target}")
                } else {
                    format!("{fd}{op}{target}")
                }
            }
            RedirectKind::HereDoc => {
                let Some(here_doc) = &redirect.here_doc else {
                    return;
                };
                let delimiter = here_doc
                    .delimiter
                    .trim_matches(|c| c == '\\' || c == '\'' || c == '"')
                    .to_string();
                let header = if here_doc.expand_vars {
                    delimiter.clone()
                } else {
                    single_quote(&delimiter)
                };
                self.pending_heredocs
                    .push((here_doc.content.clone(), delimiter));
                format!(
                    "{}<<{}{}",
                    fd_prefix(0),
                    if here_doc.strip_tabs { "-" } else { "" },
                    header
                )
            }
        };
        self.out.push_str(&text);
    }
}

fn assignment(assign: &Assignment) -> String {
    let mut text = assign.name.clone();
    if let Some(index) = &assign.index {
        text.push('[');
        text.push_str(index);
        text.push(']');
    }
    if assign.append {
        text.push('+');
    }
    text.push('=');
    match &assign.array_value {
        Some(elements) => {
            let printed: Vec<String> = elements.iter().map(|w| unquoted_word(w, false)).collect();
            text.push('(');
            text.push_str(&printed.join(" "));
            text.push(')');
        }
        None => {
            if !is_empty_word(&assign.value) {
                text.push_str(&WordWriter::render(&assign.value, false, true));
            }
        }
    }
    text
}

/// Operator words of a `[[ ]]` expression, which the parser stores as bare
/// literals and which must be printed without escaping.
fn conditional_operator(word: &Word) -> Option<&str> {
    match word.as_slice() {
        [WordPart::Literal(op)]
            if matches!(op.as_str(), "&&" | "||" | "(" | ")" | "<" | ">" | "!") =>
        {
            Some(op)
        }
        _ => None,
    }
}

/// The right operand of `=~`. The parser reads it from the raw source and only
/// protects glob and brace characters, so its unquoted text is regex syntax and
/// is printed verbatim; quoted parts keep their quotes.
fn regex_operand(word: &Word) -> String {
    let mut out = String::new();
    for part in word {
        match part {
            WordPart::Literal(text) => out.push_str(text),
            other => out.push_str(&WordWriter::render(
                std::slice::from_ref(other),
                false,
                true,
            )),
        }
    }
    out
}

fn is_empty_word(word: &Word) -> bool {
    word.iter()
        .all(|part| matches!(part, WordPart::Literal(text) if text.is_empty()))
}

/// Print a word outside of any quotes. `command_position` marks the first word
/// of a simple command, where reserved words and `NAME=` prefixes would change
/// the command's meaning if they were printed bare.
fn unquoted_word(word: &Word, command_position: bool) -> String {
    if is_empty_word(word) {
        return "''".to_string();
    }
    let mut text = WordWriter::render(word, command_position, false);
    if command_position {
        if is_reserved_word(&text) {
            text.insert(0, '\\');
        } else if let Some(split) = assignment_prefix_len(&text) {
            text.insert(split, '\\');
        }
    }
    text
}

/// When `text` starts like an assignment (`NAME=` or `NAME+=`), the byte offset
/// of the operator, where a backslash keeps it a plain command word.
fn assignment_prefix_len(text: &str) -> Option<usize> {
    let mut chars = text.char_indices();
    let (_, first) = chars.next()?;
    if !(first.is_alphabetic() || first == '_') {
        return None;
    }
    for (offset, c) in chars {
        if c.is_alphanumeric() || c == '_' {
            continue;
        }
        let rest = &text[offset..];
        return (rest.starts_with('=') || rest.starts_with("+=")).then_some(offset);
    }
    None
}

/// Renders the parts of one word, tracking just enough context to quote each
/// character correctly: the first character of the word, the previous output
/// character, and whether we are inside an extended-glob group.
struct WordWriter {
    out: String,
    command_position: bool,
    /// Assignment values: a leading `~` there is a real tilde expansion.
    assignment_value: bool,
    /// Last character written unescaped; `?*+@!` before `(` open an extglob.
    last_unescaped: Option<char>,
    extglob_depth: usize,
}

impl WordWriter {
    fn render(parts: &[WordPart], command_position: bool, assignment_value: bool) -> String {
        let mut writer = WordWriter {
            out: String::new(),
            command_position,
            assignment_value,
            last_unescaped: None,
            extglob_depth: 0,
        };
        for (index, part) in parts.iter().enumerate() {
            writer.part(part, parts.get(index + 1));
        }
        writer.out
    }

    fn raw(&mut self, text: &str) {
        self.out.push_str(text);
        self.last_unescaped = text.chars().last();
    }

    fn escaped(&mut self, c: char) {
        self.out.push('\\');
        self.out.push(c);
        self.last_unescaped = None;
    }

    fn part(&mut self, part: &WordPart, next: Option<&WordPart>) {
        match part {
            WordPart::Literal(text) => self.literal(text),
            WordPart::SingleQuoted(text) => self.raw(&single_quote(text)),
            WordPart::DoubleQuoted(parts) => {
                let inner = double_quoted_inner(parts);
                self.raw(&format!("\"{inner}\""));
            }
            WordPart::Variable(name) => {
                let next_text =
                    next.map(|part| WordWriter::render(std::slice::from_ref(part), false, true));
                self.raw(&variable(name, next_text.as_deref()));
            }
            WordPart::VariablePath { name, path } => {
                let mut text = format!("${name}");
                path_suffix(&mut text, path);
                self.raw(&text);
            }
            WordPart::Interpolated(parts) => self.raw(&interpolated(parts)),
            WordPart::Closure { params, body_src } => {
                self.raw(&format!("{{|{}| {}}}", params.join(" "), body_src));
            }
            WordPart::CommandSub(command) => self.raw(&format!("$({command})")),
            WordPart::Glob(pattern) => self.raw(pattern),
            WordPart::Tilde(user) => self.raw(&format!("~{user}")),
            WordPart::BraceExpansion(items) => {
                let items: Vec<String> = items
                    .iter()
                    .map(|item| WordWriter::render(item, false, true))
                    .collect();
                self.raw(&format!("{{{}}}", items.join(",")));
            }
            WordPart::BraceRange { start, end, step } => match step {
                Some(step) => self.raw(&format!("{{{start}..{end}..{step}}}")),
                None => self.raw(&format!("{{{start}..{end}}}")),
            },
            WordPart::Arithmetic(expr) => self.raw(&format!("$(({expr}))")),
            WordPart::ProcessSub(command, kind) => {
                let sigil = match kind {
                    ProcessSubKind::Input => '<',
                    ProcessSubKind::Output => '>',
                };
                self.raw(&format!("{sigil}({command})"));
            }
        }
    }

    fn literal(&mut self, text: &str) {
        for c in text.chars() {
            let at_start = self.out.is_empty();
            if self.extglob_depth > 0 {
                // Inside `@( ... )` the lexer keeps blanks and operators in the
                // word, so only quoting and expansion characters need escapes.
                match c {
                    '(' => self.extglob_depth += 1,
                    ')' => self.extglob_depth -= 1,
                    _ => {}
                }
                if matches!(c, '(' | ')' | ' ' | '\t' | '|' | '&' | ';' | '<' | '>') {
                    self.raw(&c.to_string());
                    continue;
                }
            } else if c == '(' && matches!(self.last_unescaped, Some('?' | '*' | '+' | '@' | '!')) {
                self.extglob_depth = 1;
                self.raw("(");
                continue;
            }
            match c {
                '\n' => {
                    // A backslash-newline is a line continuation, so a literal
                    // newline has to be quoted.
                    self.out.push_str("'\n'");
                    self.last_unescaped = None;
                }
                ' ' | '\t' | '|' | '&' | ';' | '(' | ')' | '<' | '>' | '\'' | '"' | '\\' | '$'
                | '`' | '*' | '?' | '[' | '{' | '}' => self.escaped(c),
                '#' if at_start => self.escaped(c),
                '~' if at_start && !self.assignment_value => self.escaped(c),
                // `!` alone at the start of a command is pipeline negation, and
                // `!(` there is negation of a subshell.
                '!' if at_start && self.command_position => self.escaped(c),
                _ => self.raw(&c.to_string()),
            }
        }
    }
}

/// `$name` when the name cannot run into the following text, else `${name}`.
fn variable(name: &str, next: Option<&str>) -> String {
    let simple = is_identifier(name)
        || (name.chars().count() == 1
            && name
                .chars()
                .all(|c| "?$!#@*-".contains(c) || c.is_ascii_digit()));
    let next_extends = |next: &str| {
        let mut chars = next.chars();
        match chars.next() {
            Some(c) if c.is_alphanumeric() || c == '_' || c == '[' => true,
            // `$name.field` is jsh's structured path syntax.
            Some('.') => chars
                .next()
                .is_some_and(|c| c.is_alphanumeric() || c == '_'),
            _ => false,
        }
    };
    if simple && !(is_identifier(name) && next.is_some_and(next_extends)) {
        format!("${name}")
    } else {
        format!("${{{name}}}")
    }
}

fn is_identifier(name: &str) -> bool {
    let mut chars = name.chars();
    matches!(chars.next(), Some(c) if c.is_alphabetic() || c == '_')
        && chars.all(|c| c.is_alphanumeric() || c == '_')
}

fn path_suffix(text: &mut String, path: &[PathSeg]) {
    for segment in path {
        match segment {
            PathSeg::Field(field) => {
                text.push('.');
                text.push_str(field);
            }
            PathSeg::Index(index) => text.push_str(&format!("[{index}]")),
        }
    }
}

/// The text between the quotes of a `"..."` word part.
fn double_quoted_inner(parts: &[WordPart]) -> String {
    let mut out = String::new();
    for (index, part) in parts.iter().enumerate() {
        match part {
            WordPart::Literal(text) | WordPart::SingleQuoted(text) => {
                for c in text.chars() {
                    if matches!(c, '\\' | '"' | '$' | '`') {
                        out.push('\\');
                    }
                    out.push(c);
                }
            }
            WordPart::Variable(name) => {
                let next = parts
                    .get(index + 1)
                    .map(|next| double_quoted_inner(std::slice::from_ref(next)));
                out.push_str(&variable(name, next.as_deref()));
            }
            WordPart::VariablePath { name, path } => {
                out.push('$');
                out.push_str(name);
                path_suffix(&mut out, path);
            }
            WordPart::CommandSub(command) => out.push_str(&format!("$({command})")),
            WordPart::Arithmetic(expr) => out.push_str(&format!("$(({expr}))")),
            WordPart::DoubleQuoted(inner) => out.push_str(&double_quoted_inner(inner)),
            // The parser never puts anything else inside double quotes; keep
            // whatever text the part stands for rather than dropping it.
            other => {
                for c in WordWriter::render(std::slice::from_ref(other), false, true).chars() {
                    if matches!(c, '\\' | '"' | '$' | '`') {
                        out.push('\\');
                    }
                    out.push(c);
                }
            }
        }
    }
    out
}

fn interpolated(parts: &[InterpPart]) -> String {
    let mut out = String::from("$\"");
    for part in parts {
        match part {
            InterpPart::Lit(text) => {
                for c in text.chars() {
                    match c {
                        '\\' | '"' | '(' | '$' => {
                            out.push('\\');
                            out.push(c);
                        }
                        '\n' => out.push_str("\\n"),
                        '\t' => out.push_str("\\t"),
                        '\r' => out.push_str("\\r"),
                        _ => out.push(c),
                    }
                }
            }
            InterpPart::Expr(word) => {
                out.push('(');
                out.push_str(&WordWriter::render(word, false, true));
                out.push(')');
            }
        }
    }
    out.push('"');
    out
}

/// Bash's `sh_single_quote`: `'...'`, with each `'` written as `'\''`.
pub(crate) fn single_quote(text: &str) -> String {
    format!("'{}'", text.replace('\'', "'\\''"))
}

fn is_reserved_word(word: &str) -> bool {
    matches!(
        word,
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
            | "select"
            | "time"
            | "coproc"
            | "!"
            | "{"
            | "}"
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::parser::parse;

    /// Parse `source`, which must define exactly one function, and return it.
    fn function_of(source: &str) -> (String, CompoundCommand) {
        let program = parse(source).unwrap_or_else(|e| panic!("parse {source:?}: {e}"));
        assert_eq!(program.len(), 1, "{source:?}");
        match &program[0].list.first.commands[..] {
            [Command::FunctionDef { name, body }] => (name.clone(), (**body).clone()),
            other => panic!("not a function definition: {other:?}"),
        }
    }

    fn format_source(source: &str) -> String {
        let (name, body) = function_of(source);
        format_function(&name, &body)
    }

    /// The text bash 5.2 prints for `declare -f` of each definition, checked
    /// against `bash --norc --noprofile -c 'source FILE; declare -f'`.
    const BASH_LAYOUT: &[(&str, &str)] = &[
        ("f() { echo hi; }", "f () \n{ \n    echo hi\n}"),
        (
            "f() { if [ -n \"$1\" ]; then echo yes; elif [[ $x == a* ]]; then echo elif; else echo no; fi; }",
            "f () \n{ \n    if [ -n \"$1\" ]; then\n        echo yes;\n    else\n        if [[ $x == a* ]]; then\n            echo elif;\n        else\n            echo no;\n        fi;\n    fi\n}",
        ),
        (
            "f() { for i in a \"b c\" $d; do echo $i; done; for j; do :; done; }",
            "f () \n{ \n    for i in a \"b c\" $d;\n    do\n        echo $i;\n    done;\n    for j in \"$@\";\n    do\n        :;\n    done\n}",
        ),
        (
            "f() { while read -r line; do echo \"$line\"; done < file; until false; do break; done; }",
            "f () \n{ \n    while read -r line; do\n        echo \"$line\";\n    done < file;\n    until false; do\n        break;\n    done\n}",
        ),
        (
            "f() { case $1 in a|b) echo ab;; c) echo c;& d) echo d;;& *) echo other;; esac; }",
            "f () \n{ \n    case $1 in \n        a | b)\n            echo ab\n        ;;\n        c)\n            echo c\n        ;&\n        d)\n            echo d\n        ;;&\n        *)\n            echo other\n        ;;\n    esac\n}",
        ),
        (
            "f() { ( cd /tmp && ls ) | grep x || echo none; ! true; }",
            "f () \n{ \n    ( cd /tmp && ls ) | grep x || echo none;\n    ! true\n}",
        ),
        (
            "f() { local x=1 y; x+=2; arr=(1 \"2 3\"); arr[1]=z; echo ${x:-def} ${#arr[@]} \"${arr[@]}\" $(date) $((1+2)); }",
            "f () \n{ \n    local x=1 y;\n    x+=2;\n    arr=(1 \"2 3\");\n    arr[1]=z;\n    echo ${x:-def} ${#arr[@]} \"${arr[@]}\" $(date) $((1+2))\n}",
        ),
        (
            "f() { for ((i=0; i<3; i++)); do echo $i; done; (( x++ )); }",
            "f () \n{ \n    for ((i=0; i<3; i++))\n    do\n        echo $i;\n    done;\n    (( x++ ))\n}",
        ),
        (
            "f() { cat <<EOT\nhello $x\nEOT\ncat <<'Q'\nraw $y\nQ\necho after; }",
            "f () \n{ \n    cat <<EOT\nhello $x\nEOT\n\n    cat <<'Q'\nraw $y\nQ\n\n    echo after\n}",
        ),
        (
            "f() { echo a >out 2>&1; echo b >>out; cat <in; echo c 2>/dev/null >&2; echo d &> all; cmd <<< \"here\"; exec 3<&0; }",
            "f () \n{ \n    echo a > out 2>&1;\n    echo b >> out;\n    cat < in;\n    echo c 2> /dev/null 1>&2;\n    echo d &> all;\n    cmd <<< \"here\";\n    exec 3<&0\n}",
        ),
        (
            "f() { sleep 1 & wait; { echo grouped; echo two; } > out; }",
            "f () \n{ \n    sleep 1 & wait;\n    { \n        echo grouped;\n        echo two\n    } > out\n}",
        ),
        (
            "f() { echo 'single' \"dou\\\"ble\" \\$escaped; }",
            "f () \n{ \n    echo 'single' \"dou\\\"ble\" \\$escaped\n}",
        ),
        (
            "f() { g() { echo inner; }; g; }",
            "f () \n{ \n    function g () \n    { \n        echo inner\n    };\n    g\n}",
        ),
        ("f() { x=1 y=2 cmd arg; }", "f () \n{ \n    x=1 y=2 cmd arg\n}"),
        (
            "f() { [[ -f x && ( -d y || ! -e z ) ]]; [[ $a =~ ^[0-9]+$ ]]; }",
            "f () \n{ \n    [[ -f x && ( -d y || ! -e z ) ]];\n    [[ $a =~ ^[0-9]+$ ]]\n}",
        ),
        (
            "f() { select x in a b; do echo $x; done; }",
            "f () \n{ \n    select x in a b;\n    do\n        echo $x;\n    done\n}",
        ),
        (
            "f() { echo {a,b} {1..3} ~ ~/x *.txt; }",
            "f () \n{ \n    echo {a,b} {1..3} ~ ~/x *.txt\n}",
        ),
        (
            "f() { cat <<-EOT\n\ttabbed\n\tEOT\n}",
            "f () \n{ \n    cat <<-EOT\ntabbed\nEOT\n\n}",
        ),
        (
            "f() { a | b | c; a && b || c; a; b & c; }",
            "f () \n{ \n    a | b | c;\n    a && b || c;\n    a;\n    b & c\n}",
        ),
        (
            "f() { if true; then cat <<EOF1\nx\nEOF1\nfi; }",
            "f () \n{ \n    if true; then\n        cat <<EOF1\nx\nEOF1\n\n    fi\n}",
        ),
        (
            "f() { cat <<EOF2 && echo y\nbody\nEOF2\n}",
            "f () \n{ \n    cat <<EOF2 && \nbody\nEOF2\n echo y\n}",
        ),
        (
            "f() { while read l; do echo $l; done <<EOF5\nin\nEOF5\n}",
            "f () \n{ \n    while read l; do\n        echo $l;\n    done <<EOF5\nin\nEOF5\n\n}",
        ),
        ("f() { sleep 1 & }", "f () \n{ \n    sleep 1 &\n}"),
        ("f() { a & b & }", "f () \n{ \n    a & b &\n}"),
        ("f() ( echo sub )", "f () \n{ \n    ( echo sub )\n}"),
        ("f() { echo; } > out", "f () \n{ \n    echo\n} > out"),
        (
            "f() { case x in a) ;; b) echo b; echo c;; esac; }",
            "f () \n{ \n    case x in \n        a)\n\n        ;;\n        b)\n            echo b;\n            echo c\n        ;;\n    esac\n}",
        ),
        (
            "f() { for ((;;)); do break; done; }",
            "f () \n{ \n    for ((1; 1; 1))\n    do\n        break;\n    done\n}",
        ),
        (
            "f() { { a; b; }; ( c; d ); }",
            "f () \n{ \n    { \n        a;\n        b\n    };\n    ( c;\n    d )\n}",
        ),
        (
            "f() { if a; b; then c; fi; while a; b; do c; done; }",
            "f () \n{ \n    if a;\n    b; then\n        c;\n    fi;\n    while a;\n    b; do\n        c;\n    done\n}",
        ),
        (
            "f() { echo ${10} ${#x} \"${x}y\" \"${a[1]}\" $@ \"$@\" $* $# $? $$ $! $- $0; }",
            "f () \n{ \n    echo ${10} ${#x} \"${x}y\" \"${a[1]}\" $@ \"$@\" $* $# $? $$ $! $- $0\n}",
        ),
        (
            "f() { echo \"a\\$b\\`c\\\\d\\\"e\" 'it'\\''s' a\\ b; }",
            "f () \n{ \n    echo \"a\\$b\\`c\\\\d\\\"e\" 'it'\\''s' a\\ b\n}",
        ),
        (
            "f() { ! a | b; time sleep 1; }",
            "f () \n{ \n    ! a | b;\n    time sleep 1\n}",
        ),
        (
            "f() { declare -A m=([k]=v [j]=w); local -a x=(1 \"2 3\"); }",
            "f () \n{ \n    declare -A m=([k]=v [j]=w);\n    local -a x=(1 \"2 3\")\n}",
        ),
        // Deliberately not bash's `coproc COPROC cat`, which re-evaluates as a
        // command named COPROC; see `Printer::compound`.
        ("f() { coproc cat; }", "f () \n{ \n    coproc cat\n}"),
        (
            "f() { coproc C { cat; }; }",
            "f () \n{ \n    coproc C { \n        cat\n    }\n}",
        ),
        (
            "f() { time -p true | cat; time ! false; }",
            "f () \n{ \n    time -p true | cat;\n    time ! false\n}",
        ),
        (
            "f() { exec {fd}>/tmp/x; echo hi >&$fd; }",
            "f () \n{ \n    exec {fd}> /tmp/x;\n    echo hi >&$fd\n}",
        ),
        (
            "f() { echo a; # comment\necho b; }",
            "f () \n{ \n    echo a;\n    echo b\n}",
        ),
    ];

    #[test]
    fn matches_bash_declare_f_layout() {
        for (source, expected) in BASH_LAYOUT {
            assert_eq!(&format_source(source), expected, "source: {source:?}");
        }
    }

    /// Definitions whose printed form is not bash's text byte for byte (jsh
    /// keeps no source text, so `\`pwd\`` becomes `$(pwd)` and `$'\n'` a
    /// single-quoted newline) but which must still round-trip.
    const EXTRA_CORPUS: &[&str] = &[
        "f() { echo `pwd` $'ansi\\n' $'tab\\there'; }",
        "f() { echo \"$file.txt\" ${file}.txt $x$y ${x}_y \"${x}_y\" $x-y; }",
        "f() { echo \\* \\? \\[ a\\ b \\# \\~ '#' x#y \\$HOME \\\\; }",
        "f() { \\if; \\a=b; 'x=y' z; \\time; }",
        "f() { echo \"\" '' a\"\"b; x=; y=''; }",
        "f() { echo $\"hi (echo x) \\(lit\\) end\"; }",
        "f() { echo <(ls) >(cat) {x..z..2}; }",
        "f() { case $x in @(a|b)) echo one;; *(x|y)) echo two;; esac; echo !(z) +(a|b)c; }",
        "f() { [[ $x == \"a*\" && $y != 'b' || ( -n $z ) ]]; [[ abc =~ \"a.c\" ]]; [[ $x =~ ^a\"$q\"(b|c)$ ]]; }",
        "f() { [[ a < b ]]; [[ b > a ]]; [ -n x ]; test x = y; }",
        "f() { declare -A m=([k]=v [j]=w); m[k]+=x; arr+=(z); }",
        "f() { exec 2>&-; exec 5>file 6<in; echo x >| clobber; echo y &>> both; }",
        "f() { cmd1 &! cmd2 & cmd3; }",
        "f() { { cat; } <<EOF\nbody $x\nEOF\n}",
        "f() { echo a\nb() { echo nested; }\nc; }",
        "function g { echo kw; }",
        "f() { while :; do case $1 in -a) shift;; --) break;; *) return 1;; esac; done; }",
        "f() { if a; then b; elif c; then d; elif e; then g; fi; }",
        "f() { until (( i >= 3 )); do (( i++ )); done; }",
        "f() { echo $(echo \"$(nested \"$x\")\") $((x * (2 + 3))); }",
        "f() { echo \"tab\there\" 'new\nline' \"new\nline\"; }",
        "f() { echo é 日本 ünïcödé; }",
        "f() { cat <<'A' | grep b && echo y\nline $x\nA\n}",
        "f() { echo \"${x//a/b}\" ${x#*/} ${x%%.*} ${!ref} ${x^^} ${x:1:2}; }",
    ];

    /// Collapse adjacent literal parts: the parser merges them, so a re-parse
    /// may split text into parts differently without changing its meaning.
    fn normalize_word(word: &Word) -> Word {
        let mut out: Word = Vec::new();
        for part in word {
            let part = match part {
                WordPart::DoubleQuoted(inner) => WordPart::DoubleQuoted(normalize_word(inner)),
                other => other.clone(),
            };
            match (out.last_mut(), &part) {
                (Some(WordPart::Literal(prev)), WordPart::Literal(next)) => prev.push_str(next),
                _ => out.push(part),
            }
        }
        out
    }

    /// Rewrite the AST into the form the printer canonicalizes to, so a
    /// reparse can be compared with the original: adjacent literal text is
    /// merged (and single-quoted text is literal text), `elif` becomes a nested
    /// `if`, `for x` iterates over `"$@"`, omitted arithmetic-`for` clauses are
    /// `1`, an anonymous coprocess is `COPROC`, a function body is a brace
    /// group, and here-document delimiters lose their original quote style.
    struct Normalize;

    impl Normalize {
        fn word(word: &Word) -> Word {
            let mut out: Word = Vec::new();
            for part in word {
                let part = match part {
                    WordPart::SingleQuoted(text) => WordPart::Literal(text.clone()),
                    WordPart::DoubleQuoted(inner) => WordPart::DoubleQuoted(Self::word(inner)),
                    WordPart::BraceExpansion(items) => {
                        WordPart::BraceExpansion(items.iter().map(Self::word).collect())
                    }
                    WordPart::Interpolated(parts) => WordPart::Interpolated(
                        parts
                            .iter()
                            .map(|part| match part {
                                InterpPart::Expr(word) => InterpPart::Expr(Self::word(word)),
                                other => other.clone(),
                            })
                            .collect(),
                    ),
                    other => other.clone(),
                };
                match (out.last_mut(), &part) {
                    (Some(WordPart::Literal(prev)), WordPart::Literal(next)) => prev.push_str(next),
                    (_, WordPart::Literal(next)) if next.is_empty() && !word.is_empty() => {}
                    _ => out.push(part),
                }
            }
            if out.is_empty() {
                out.push(WordPart::Literal(String::new()));
            }
            out
        }

        fn words(words: &[Word]) -> Vec<Word> {
            words.iter().map(Self::word).collect()
        }

        fn redirects(redirects: &[Redirect]) -> Vec<Redirect> {
            redirects
                .iter()
                .map(|redirect| {
                    let mut redirect = redirect.clone();
                    redirect.target = Self::word(&redirect.target);
                    let default = match redirect.kind {
                        RedirectKind::Input | RedirectKind::HereString | RedirectKind::HereDoc => 0,
                        _ => 1,
                    };
                    if redirect.fd == Some(default) {
                        redirect.fd = None;
                    }
                    if let Some(here_doc) = &mut redirect.here_doc {
                        if redirect.kind == RedirectKind::HereDoc {
                            here_doc.delimiter = here_doc
                                .delimiter
                                .trim_matches(|c| c == '\\' || c == '\'' || c == '"')
                                .to_string();
                            redirect.target = vec![WordPart::Literal(here_doc.delimiter.clone())];
                        }
                    }
                    redirect
                })
                .collect()
        }

        fn list(commands: &[CompleteCommand]) -> Vec<CompleteCommand> {
            commands
                .iter()
                .map(|command| {
                    let mut command = command.clone();
                    command.list.first = Self::pipeline(&command.list.first);
                    for (_, pipeline) in &mut command.list.rest {
                        *pipeline = Self::pipeline(pipeline);
                    }
                    command
                })
                .collect()
        }

        fn pipeline(pipeline: &Pipeline) -> Pipeline {
            Pipeline {
                negated: pipeline.negated,
                commands: pipeline.commands.iter().map(Self::command).collect(),
                timed: pipeline.timed,
            }
        }

        fn simple(simple: &SimpleCommand) -> SimpleCommand {
            let mut simple = simple.clone();
            for assign in &mut simple.assignments {
                assign.value = Self::word(&assign.value);
                if let Some(values) = &assign.array_value {
                    assign.array_value = Some(Self::words(values));
                }
            }
            simple.words = Self::words(&simple.words);
            simple.redirects = Self::redirects(&simple.redirects);
            // Source positions and recorded text change with the layout.
            simple.line = 0;
            simple.text.clear();
            simple
        }

        fn command(command: &Command) -> Command {
            match command {
                Command::Simple(simple) => Command::Simple(Self::simple(simple)),
                Command::Compound(compound) => Command::Compound(Self::compound(compound)),
                Command::FunctionDef { name, body } => Command::FunctionDef {
                    name: name.clone(),
                    body: Box::new(Self::function_body(body)),
                },
            }
        }

        fn function_body(body: &CompoundCommand) -> CompoundCommand {
            match Self::compound(body) {
                group @ CompoundCommand::BraceGroup { .. } => group,
                other => CompoundCommand::BraceGroup {
                    body: vec![CompleteCommand {
                        list: crate::parser::ast::AndOrList {
                            first: Pipeline {
                                negated: false,
                                commands: vec![Command::Compound(other)],
                                timed: None,
                            },
                            rest: Vec::new(),
                        },
                        background: false,
                        disown: false,
                    }],
                    redirects: Vec::new(),
                },
            }
        }

        fn compound(compound: &CompoundCommand) -> CompoundCommand {
            use CompoundCommand as C;
            match compound {
                C::BraceGroup { body, redirects } => C::BraceGroup {
                    body: Self::list(body),
                    redirects: Self::redirects(redirects),
                },
                C::Subshell { body, redirects } => C::Subshell {
                    body: Self::list(body),
                    redirects: Self::redirects(redirects),
                },
                C::If {
                    conditions,
                    else_branch,
                    redirects,
                } => {
                    let mut else_branch = else_branch.as_deref().map(Self::list);
                    for (condition, body) in conditions.iter().skip(1).rev() {
                        let nested = C::If {
                            conditions: vec![(Self::list(condition), Self::list(body))],
                            else_branch,
                            redirects: Vec::new(),
                        };
                        else_branch = Some(vec![CompleteCommand {
                            list: crate::parser::ast::AndOrList {
                                first: Pipeline {
                                    negated: false,
                                    commands: vec![Command::Compound(nested)],
                                    timed: None,
                                },
                                rest: Vec::new(),
                            },
                            background: false,
                            disown: false,
                        }]);
                    }
                    let (condition, body) = &conditions[0];
                    C::If {
                        conditions: vec![(Self::list(condition), Self::list(body))],
                        else_branch,
                        redirects: Self::redirects(redirects),
                    }
                }
                C::For {
                    var,
                    words,
                    body,
                    redirects,
                    ..
                } => C::For {
                    line: 0,
                    text: String::new(),
                    var: var.clone(),
                    words: Some(match words {
                        Some(words) => Self::words(words),
                        None => vec![vec![WordPart::DoubleQuoted(vec![WordPart::Variable(
                            "@".into(),
                        )])]],
                    }),
                    body: Self::list(body),
                    redirects: Self::redirects(redirects),
                },
                C::Select {
                    var,
                    words,
                    body,
                    redirects,
                    ..
                } => C::Select {
                    line: 0,
                    text: String::new(),
                    var: var.clone(),
                    words: Some(match words {
                        Some(words) => Self::words(words),
                        None => vec![vec![WordPart::DoubleQuoted(vec![WordPart::Variable(
                            "@".into(),
                        )])]],
                    }),
                    body: Self::list(body),
                    redirects: Self::redirects(redirects),
                },
                C::CStyleFor {
                    init,
                    condition,
                    update,
                    body,
                    redirects,
                } => {
                    let or_one = |text: &String| {
                        if text.trim().is_empty() {
                            "1".to_string()
                        } else {
                            text.clone()
                        }
                    };
                    C::CStyleFor {
                        init: or_one(init),
                        condition: or_one(condition),
                        update: or_one(update),
                        body: Self::list(body),
                        redirects: Self::redirects(redirects),
                    }
                }
                C::While {
                    condition,
                    body,
                    redirects,
                } => C::While {
                    condition: Self::list(condition),
                    body: Self::list(body),
                    redirects: Self::redirects(redirects),
                },
                C::Until {
                    condition,
                    body,
                    redirects,
                } => C::Until {
                    condition: Self::list(condition),
                    body: Self::list(body),
                    redirects: Self::redirects(redirects),
                },
                C::Case {
                    word,
                    arms,
                    redirects,
                    ..
                } => C::Case {
                    line: 0,
                    text: String::new(),
                    word: Self::word(word),
                    arms: arms
                        .iter()
                        .map(|arm| CaseArm {
                            patterns: Self::words(&arm.patterns),
                            body: Self::list(&arm.body),
                            terminator: arm.terminator.clone(),
                        })
                        .collect(),
                    redirects: Self::redirects(redirects),
                },
                C::Arithmetic { expr, redirects } => C::Arithmetic {
                    expr: expr.trim().to_string(),
                    redirects: Self::redirects(redirects),
                },
                C::Coproc {
                    name,
                    command,
                    redirects,
                } => C::Coproc {
                    name: Some(name.clone().unwrap_or_else(|| "COPROC".into())),
                    command: Box::new(Self::command(command)),
                    redirects: Self::redirects(redirects),
                },
            }
        }
    }

    #[test]
    fn printed_functions_reparse_to_an_equivalent_ast_and_print_identically() {
        let sources: Vec<&str> = BASH_LAYOUT
            .iter()
            .map(|(source, _)| *source)
            .chain(EXTRA_CORPUS.iter().copied())
            .collect();
        for source in sources {
            let (name, original) = function_of(source);
            let first = format_function(&name, &original);
            let (reparsed_name, reparsed) = function_of(&first);
            assert_eq!(reparsed_name, name, "{source:?}");
            assert_eq!(
                Normalize::function_body(&reparsed),
                Normalize::function_body(&original),
                "{source:?} printed as {first:?}"
            );
            let second = format_function(&reparsed_name, &reparsed);
            assert_eq!(first, second, "unstable for {source:?}");
        }
    }

    #[test]
    fn printed_words_reparse_to_the_same_parts() {
        let words = [
            "plain",
            "a\\ b",
            "\\*\\?\\[x]",
            "\"$x\"",
            "'it'\\''s'",
            "pre${x}post",
            "$x.txt",
            "${x}.txt",
            "\"${x}_y\"",
            "a\\$b",
            "\\~user",
            "~user/dir",
            "*.rs",
            "[ab]c",
            "{a,b}c",
            "{1..5}",
            "$(cmd \"arg\")",
            "\"a\\\\b\\\"c\\$d\\`e\"",
            "x\\#y",
            "\\#y",
            "@(a|b)",
            "!(x)",
            "+(a|b)c",
            "\\@\\(a\\)",
        ];
        for raw in words {
            let parts = crate::parser::parse_word_parts(raw);
            let printed = unquoted_word(&parts, false);
            let reparsed = crate::parser::parse_word_parts(&printed);
            assert_eq!(
                normalize_word(&reparsed),
                normalize_word(&parts),
                "{raw:?} printed as {printed:?}"
            );
        }
    }

    #[test]
    fn command_position_keeps_words_plain() {
        let program = parse("\\if a; \\a=b c; \\! x").unwrap();
        for (command, expected) in program.iter().zip(["\\if a", "a\\=b c", "\\! x"]) {
            let Command::Simple(simple) = &command.list.first.commands[0] else {
                panic!("not simple");
            };
            let mut printer = Printer::default();
            printer.simple(simple);
            assert_eq!(printer.out, expected);
        }
    }

    #[test]
    fn reserved_function_names_use_the_function_keyword() {
        let body = CompoundCommand::BraceGroup {
            body: parse("true").unwrap(),
            redirects: Vec::new(),
        };
        assert_eq!(
            format_function("if", &body),
            "function if () \n{ \n    true\n}"
        );
    }
}
