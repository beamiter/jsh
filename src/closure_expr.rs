use crate::parser::ast::PathSeg;
/// Phase 8a — minimal expression evaluator for closure bodies.
///
/// Sits between the "literal JSON body" shortcut and the full command-pipeline
/// interpreter in `apply_closure`. Recognizes a tiny expression language so
/// that `{|a, b| $a + $b}` and `{|r| $r.age > 30}` work without invoking the
/// shell parser.
///
/// Grammar (precedence low → high):
///   or  := and ('||' and)*
///   and := cmp ('&&' cmp)*
///   cmp := add (('=='|'!='|'<'|'>'|'<='|'>=') add)?
///   add := mul (('+'|'-') mul)*
///   mul := unary (('*'|'/'|'%') unary)*
///   unary := ('!'|'-') unary | primary
///   primary := number | string | true|false|null | '$' name path? | '(' or ')'
use crate::value::Value;
use std::cell::RefCell;
use std::collections::HashMap;
use std::sync::Arc;

#[derive(Debug, Clone, PartialEq)]
enum Tok {
    Int(u64), // unsigned magnitude permits the literal -9223372036854775808
    OutOfRangeInt,
    Float(f64),
    Str(String),
    Ident(String), // true|false|null
    Var(String, Vec<PathSeg>),
    Plus,
    Minus,
    Star,
    Slash,
    Percent,
    Eq,
    Ne,
    Lt,
    Gt,
    Le,
    Ge,
    AndAnd,
    OrOr,
    Bang,
    LParen,
    RParen,
    LBrace,
    RBrace,
    Comma,
    FatArrow,
}

fn tokenize(src: &str) -> Result<Vec<Tok>, String> {
    let mut out = Vec::new();
    let bytes = src.as_bytes();
    let mut i = 0;
    while i < bytes.len() {
        let c = bytes[i];
        match c {
            b' ' | b'\t' | b'\n' | b'\r' => {
                i += 1;
            }
            b'+' => {
                out.push(Tok::Plus);
                i += 1;
            }
            b'-' => {
                out.push(Tok::Minus);
                i += 1;
            }
            b'*' => {
                out.push(Tok::Star);
                i += 1;
            }
            b'/' => {
                out.push(Tok::Slash);
                i += 1;
            }
            b'%' => {
                out.push(Tok::Percent);
                i += 1;
            }
            b'(' => {
                out.push(Tok::LParen);
                i += 1;
            }
            b')' => {
                out.push(Tok::RParen);
                i += 1;
            }
            b'{' => {
                out.push(Tok::LBrace);
                i += 1;
            }
            b'}' => {
                out.push(Tok::RBrace);
                i += 1;
            }
            b'=' if i + 1 < bytes.len() && bytes[i + 1] == b'=' => {
                out.push(Tok::Eq);
                i += 2;
            }
            b'=' if i + 1 < bytes.len() && bytes[i + 1] == b'>' => {
                out.push(Tok::FatArrow);
                i += 2;
            }
            b',' => {
                out.push(Tok::Comma);
                i += 1;
            }
            b'!' if i + 1 < bytes.len() && bytes[i + 1] == b'=' => {
                out.push(Tok::Ne);
                i += 2;
            }
            b'<' if i + 1 < bytes.len() && bytes[i + 1] == b'=' => {
                out.push(Tok::Le);
                i += 2;
            }
            b'>' if i + 1 < bytes.len() && bytes[i + 1] == b'=' => {
                out.push(Tok::Ge);
                i += 2;
            }
            b'<' => {
                out.push(Tok::Lt);
                i += 1;
            }
            b'>' => {
                out.push(Tok::Gt);
                i += 1;
            }
            b'&' if i + 1 < bytes.len() && bytes[i + 1] == b'&' => {
                out.push(Tok::AndAnd);
                i += 2;
            }
            b'|' if i + 1 < bytes.len() && bytes[i + 1] == b'|' => {
                out.push(Tok::OrOr);
                i += 2;
            }
            b'!' => {
                out.push(Tok::Bang);
                i += 1;
            }
            b'"' => {
                let start = i + 1;
                let mut j = start;
                let mut s = String::new();
                while j < bytes.len() && bytes[j] != b'"' {
                    if bytes[j] == b'\\' && j + 1 < bytes.len() {
                        let escaped = src[j + 1..]
                            .chars()
                            .next()
                            .expect("a byte after a backslash starts a UTF-8 character");
                        match escaped {
                            'n' => s.push('\n'),
                            't' => s.push('\t'),
                            'r' => s.push('\r'),
                            '\\' => s.push('\\'),
                            '"' => s.push('"'),
                            other => {
                                s.push('\\');
                                s.push(other);
                            }
                        }
                        j += 1 + escaped.len_utf8();
                    } else {
                        let ch = src[j..]
                            .chars()
                            .next()
                            .expect("a source byte index starts a UTF-8 character");
                        s.push(ch);
                        j += ch.len_utf8();
                    }
                }
                if j >= bytes.len() {
                    return Err("unterminated string".to_string());
                }
                out.push(Tok::Str(s));
                i = j + 1;
            }
            b'$' => {
                // $name(.field|[N])*
                let mut j = i + 1;
                while j < bytes.len() && (bytes[j].is_ascii_alphanumeric() || bytes[j] == b'_') {
                    j += 1;
                }
                if j == i + 1 {
                    return Err("expected variable name after $".to_string());
                }
                let name = std::str::from_utf8(&bytes[i + 1..j]).unwrap().to_string();
                let mut path = Vec::new();
                while j < bytes.len() {
                    if bytes[j] == b'.' {
                        let mut k = j + 1;
                        while k < bytes.len()
                            && (bytes[k].is_ascii_alphanumeric() || bytes[k] == b'_')
                        {
                            k += 1;
                        }
                        if k == j + 1 {
                            break;
                        }
                        let seg = std::str::from_utf8(&bytes[j + 1..k]).unwrap();
                        if let Ok(n) = seg.parse::<i64>() {
                            path.push(PathSeg::Index(n));
                        } else {
                            path.push(PathSeg::Field(seg.to_string()));
                        }
                        j = k;
                    } else if bytes[j] == b'[' {
                        let mut k = j + 1;
                        while k < bytes.len() && bytes[k] != b']' {
                            k += 1;
                        }
                        if k >= bytes.len() {
                            return Err("unterminated [".to_string());
                        }
                        let seg = std::str::from_utf8(&bytes[j + 1..k]).unwrap();
                        let n: i64 = seg
                            .parse()
                            .map_err(|_| "bracket index must be integer".to_string())?;
                        path.push(PathSeg::Index(n));
                        j = k + 1;
                    } else {
                        break;
                    }
                }
                out.push(Tok::Var(name, path));
                i = j;
            }
            c if c.is_ascii_digit() => {
                let start = i;
                let mut j = i + 1;
                let mut is_float = false;
                while j < bytes.len() && bytes[j].is_ascii_digit() {
                    j += 1;
                }
                if j < bytes.len()
                    && bytes[j] == b'.'
                    && j + 1 < bytes.len()
                    && bytes[j + 1].is_ascii_digit()
                {
                    is_float = true;
                    j += 1;
                    while j < bytes.len() && bytes[j].is_ascii_digit() {
                        j += 1;
                    }
                }
                let s = std::str::from_utf8(&bytes[start..j]).unwrap();
                if is_float {
                    out.push(Tok::Float(
                        s.parse().map_err(|_| format!("bad number '{s}'"))?,
                    ));
                } else {
                    out.push(s.parse().map(Tok::Int).unwrap_or(Tok::OutOfRangeInt));
                }
                i = j;
            }
            c if c.is_ascii_alphabetic() || c == b'_' => {
                let start = i;
                let mut j = i + 1;
                while j < bytes.len() && (bytes[j].is_ascii_alphanumeric() || bytes[j] == b'_') {
                    j += 1;
                }
                let s = std::str::from_utf8(&bytes[start..j]).unwrap().to_string();
                out.push(Tok::Ident(s));
                i = j;
            }
            _ => return Err(format!("unexpected char '{}' at {}", c as char, i)),
        }
    }
    Ok(out)
}

struct Parser<'a> {
    toks: &'a [Tok],
    pos: usize,
    depth: usize,
}

impl<'a> Parser<'a> {
    fn peek(&self) -> Option<&Tok> {
        self.toks.get(self.pos)
    }
    fn bump(&mut self) -> Option<Tok> {
        let t = self.toks.get(self.pos).cloned();
        self.pos += 1;
        t
    }

    fn parse_or(&mut self) -> Result<Expr, String> {
        let mut e = self.parse_and()?;
        while matches!(self.peek(), Some(Tok::OrOr)) {
            self.bump();
            let r = self.parse_and()?;
            e = Expr::Or(Box::new(e), Box::new(r));
        }
        Ok(e)
    }
    fn parse_and(&mut self) -> Result<Expr, String> {
        let mut e = self.parse_cmp()?;
        while matches!(self.peek(), Some(Tok::AndAnd)) {
            self.bump();
            let r = self.parse_cmp()?;
            e = Expr::And(Box::new(e), Box::new(r));
        }
        Ok(e)
    }
    fn parse_cmp(&mut self) -> Result<Expr, String> {
        let l = self.parse_add()?;
        let op = match self.peek() {
            Some(Tok::Eq) => Some(Cmp::Eq),
            Some(Tok::Ne) => Some(Cmp::Ne),
            Some(Tok::Lt) => Some(Cmp::Lt),
            Some(Tok::Gt) => Some(Cmp::Gt),
            Some(Tok::Le) => Some(Cmp::Le),
            Some(Tok::Ge) => Some(Cmp::Ge),
            _ => None,
        };
        if let Some(op) = op {
            self.bump();
            let r = self.parse_add()?;
            Ok(Expr::Cmp(Box::new(l), op, Box::new(r)))
        } else {
            Ok(l)
        }
    }
    fn parse_add(&mut self) -> Result<Expr, String> {
        let mut e = self.parse_mul()?;
        loop {
            let op = match self.peek() {
                Some(Tok::Plus) => Bin::Add,
                Some(Tok::Minus) => Bin::Sub,
                _ => break,
            };
            self.bump();
            let r = self.parse_mul()?;
            e = Expr::Bin(Box::new(e), op, Box::new(r));
        }
        Ok(e)
    }
    fn parse_mul(&mut self) -> Result<Expr, String> {
        let mut e = self.parse_unary()?;
        loop {
            let op = match self.peek() {
                Some(Tok::Star) => Bin::Mul,
                Some(Tok::Slash) => Bin::Div,
                Some(Tok::Percent) => Bin::Mod,
                _ => break,
            };
            self.bump();
            let r = self.parse_unary()?;
            e = Expr::Bin(Box::new(e), op, Box::new(r));
        }
        Ok(e)
    }
    fn parse_unary(&mut self) -> Result<Expr, String> {
        if self.pos >= 1024 {
            return Err("expression exceeds 1024 tokens".into());
        }
        if self.depth >= 128 {
            return Err("expression nesting exceeds 128 levels".into());
        }
        self.depth += 1;
        let result = self.parse_unary_inner();
        self.depth -= 1;
        result
    }
    fn parse_unary_inner(&mut self) -> Result<Expr, String> {
        match self.peek() {
            Some(Tok::Bang) => {
                self.bump();
                let e = self.parse_unary()?;
                Ok(Expr::Not(Box::new(e)))
            }
            Some(Tok::Minus) => {
                self.bump();
                if matches!(self.peek(), Some(Tok::Int(n)) if *n == 1_u64 << 63) {
                    self.bump();
                    return Ok(Expr::Int(i64::MIN));
                }
                let e = self.parse_unary()?;
                Ok(Expr::Neg(Box::new(e)))
            }
            _ => self.parse_primary(),
        }
    }
    fn parse_primary(&mut self) -> Result<Expr, String> {
        match self.bump() {
            Some(Tok::Int(n)) => i64::try_from(n)
                .map(Expr::Int)
                .map_err(|_| "integer literal out of range".into()),
            Some(Tok::Float(n)) => Ok(Expr::Float(n)),
            Some(Tok::OutOfRangeInt) => Err("integer literal out of range".into()),
            Some(Tok::Str(s)) => Ok(Expr::Str(s)),
            Some(Tok::Ident(name)) => match name.as_str() {
                "true" => Ok(Expr::Bool(true)),
                "false" => Ok(Expr::Bool(false)),
                "null" => Ok(Expr::Null),
                "if" => self.parse_if_tail(),
                "match" => self.parse_match_tail(),
                _ => Err(format!("unknown identifier '{}'", name)),
            },
            Some(Tok::Var(name, path)) => Ok(Expr::Var(name, path)),
            Some(Tok::LParen) => {
                let e = self.parse_or()?;
                match self.bump() {
                    Some(Tok::RParen) => Ok(e),
                    _ => Err("expected )".to_string()),
                }
            }
            other => Err(format!("unexpected token {:?}", other)),
        }
    }

    /// `match` already consumed. Expects: <scrutinee> '{' (<pat> '=>' <expr>)(',' ...)* '}'
    fn parse_match_tail(&mut self) -> Result<Expr, String> {
        let scrut = self.parse_or()?;
        match self.bump() {
            Some(Tok::LBrace) => {}
            other => {
                return Err(format!(
                    "expected '{{' after match scrutinee, got {:?}",
                    other
                ))
            }
        }
        let mut arms = Vec::new();
        loop {
            // Allow trailing comma / empty body.
            if matches!(self.peek(), Some(Tok::RBrace)) {
                self.bump();
                break;
            }
            let pat = self.parse_match_pat()?;
            match self.bump() {
                Some(Tok::FatArrow) => {}
                other => return Err(format!("expected '=>' in match arm, got {:?}", other)),
            }
            let body = self.parse_or()?;
            arms.push((pat, body));
            match self.peek() {
                Some(Tok::Comma) => {
                    self.bump();
                }
                Some(Tok::RBrace) => {
                    self.bump();
                    break;
                }
                other => {
                    return Err(format!(
                        "expected ',' or '}}' between match arms, got {:?}",
                        other
                    ))
                }
            }
        }
        Ok(Expr::Match(Box::new(scrut), arms))
    }

    fn parse_match_pat(&mut self) -> Result<MatchPat, String> {
        // `_` is a normal Ident — treat it as wildcard. Allow `-N` for negative literals.
        let negate = if matches!(self.peek(), Some(Tok::Minus)) {
            self.bump();
            true
        } else {
            false
        };
        match self.bump() {
            Some(Tok::Ident(s)) if s == "_" && !negate => Ok(MatchPat::Wildcard),
            Some(Tok::Ident(s)) if !negate => match s.as_str() {
                "true" => Ok(MatchPat::Lit(Value::Bool(true))),
                "false" => Ok(MatchPat::Lit(Value::Bool(false))),
                "null" => Ok(MatchPat::Lit(Value::Null)),
                _ => Err(format!("match pattern must be literal or _, got {}", s)),
            },
            Some(Tok::Int(n)) => {
                let n = if negate && n == 1_u64 << 63 {
                    i64::MIN
                } else {
                    i64::try_from(n).map_err(|_| "integer literal out of range")?
                        * if negate { -1 } else { 1 }
                };
                Ok(MatchPat::Lit(Value::Int(n)))
            }
            Some(Tok::Float(n)) => Ok(MatchPat::Lit(Value::Float(if negate { -n } else { n }))),
            Some(Tok::OutOfRangeInt) => Err("integer literal out of range".into()),
            Some(Tok::Str(s)) if !negate => Ok(MatchPat::Lit(Value::String(s))),
            other => Err(format!("invalid match pattern: {:?}", other)),
        }
    }

    /// `if` already consumed. Expects: <cond> '{' <then> '}' 'else' '{' <else> '}'
    /// where <else> can also start with `if` (chained).
    fn parse_if_tail(&mut self) -> Result<Expr, String> {
        let cond = self.parse_or()?;
        match self.bump() {
            Some(Tok::LBrace) => {}
            _ => return Err("expected '{' after if condition".to_string()),
        }
        let then = self.parse_or()?;
        match self.bump() {
            Some(Tok::RBrace) => {}
            _ => return Err("expected '}' after if branch".to_string()),
        }
        match self.bump() {
            Some(Tok::Ident(ref s)) if s == "else" => {}
            other => return Err(format!("expected 'else', got {:?}", other)),
        }
        // Allow `else if`
        let else_e = match self.peek() {
            Some(Tok::Ident(s)) if s == "if" => {
                self.bump();
                self.parse_if_tail()?
            }
            Some(Tok::LBrace) => {
                self.bump();
                let e = self.parse_or()?;
                match self.bump() {
                    Some(Tok::RBrace) => {}
                    _ => return Err("expected '}' after else branch".to_string()),
                }
                e
            }
            _ => return Err("expected '{' or 'if' after else".to_string()),
        };
        Ok(Expr::If(Box::new(cond), Box::new(then), Box::new(else_e)))
    }
}

#[derive(Debug, Clone, Copy, PartialEq)]
enum Bin {
    Add,
    Sub,
    Mul,
    Div,
    Mod,
}
#[derive(Debug, Clone, Copy, PartialEq)]
enum Cmp {
    Eq,
    Ne,
    Lt,
    Gt,
    Le,
    Ge,
}

#[derive(Debug, Clone)]
enum Expr {
    Literal(Value),
    Int(i64),
    Float(f64),
    Str(String),
    Bool(bool),
    Null,
    Var(String, Vec<PathSeg>),
    Bin(Box<Expr>, Bin, Box<Expr>),
    Cmp(Box<Expr>, Cmp, Box<Expr>),
    And(Box<Expr>, Box<Expr>),
    Or(Box<Expr>, Box<Expr>),
    Neg(Box<Expr>),
    Not(Box<Expr>),
    If(Box<Expr>, Box<Expr>, Box<Expr>),
    /// `match <scrutinee> { <pat> => <arm>, ..., _ => <default> }`
    /// Patterns are literal values (int/float/string/bool/null) or `_` wildcard.
    /// First matching arm wins; falling off the end without a wildcard yields Null.
    Match(Box<Expr>, Vec<(MatchPat, Expr)>),
}

#[derive(Debug, Clone)]
enum MatchPat {
    Wildcard,
    Lit(Value),
}

/// Parsed expressions are immutable and can be reused across argument scopes.
#[derive(Clone, Debug)]
pub(crate) struct CompiledExpr(Arc<Expr>);

impl CompiledExpr {
    pub(crate) fn evaluate_with<'a>(
        &self,
        lookup: impl Fn(&str) -> Option<&'a Value>,
    ) -> Result<Value, String> {
        eval(&self.0, &lookup, 0)
    }
}

#[derive(Default)]
struct ExpressionCache {
    entries: HashMap<String, (Option<CompiledExpr>, u64)>,
    source_bytes: usize,
    clock: u64,
}

impl ExpressionCache {
    fn get(&mut self, source: &str) -> Option<Option<CompiledExpr>> {
        self.clock = self.clock.wrapping_add(1);
        let entry = self.entries.get_mut(source)?;
        entry.1 = self.clock;
        Some(entry.0.clone())
    }

    fn insert(&mut self, source: &str, expression: Option<CompiledExpr>) {
        const MAX_ENTRIES: usize = 128;
        const MAX_BYTES: usize = 256 * 1024;
        if source.len() > 8192 || self.entries.contains_key(source) {
            return;
        }
        while self.entries.len() >= MAX_ENTRIES || self.source_bytes + source.len() > MAX_BYTES {
            let Some(oldest) = self
                .entries
                .iter()
                .min_by_key(|(_, value)| value.1)
                .map(|(key, _)| key.clone())
            else {
                break;
            };
            self.source_bytes -= oldest.len();
            self.entries.remove(&oldest);
        }
        self.clock = self.clock.wrapping_add(1);
        self.entries
            .insert(source.to_owned(), (expression, self.clock));
        self.source_bytes += source.len();
    }
}

thread_local! {
    static EXPRESSIONS: RefCell<ExpressionCache> = RefCell::new(ExpressionCache::default());
}

/// Compile a pure expression, or return None for a shell-command body. Only
/// syntax is cached: runtime values and closure captures never enter the cache.
pub(crate) fn compile(source: &str) -> Result<Option<CompiledExpr>, String> {
    let source = source.trim();
    if let Some(cached) = EXPRESSIONS.with(|cache| cache.borrow_mut().get(source)) {
        return Ok(cached);
    }
    let expression = compile_uncached(source)?;
    EXPRESSIONS.with(|cache| cache.borrow_mut().insert(source, expression.clone()));
    Ok(expression)
}

fn compile_uncached(source: &str) -> Result<Option<CompiledExpr>, String> {
    // Share the same literal boundary between sequential and parallel closure
    // application (notably JSON Unicode escapes, arrays and records).
    if let Ok(value) = serde_json::from_str(source) {
        return Ok(Some(CompiledExpr(Arc::new(Expr::Literal(
            Value::from_json(value),
        )))));
    }
    if has_shell_syntax(source) {
        return Ok(None);
    }
    let toks = match tokenize(source) {
        Ok(tokens) => tokens,
        Err(error) if error == "integer literal out of range" => return Err(error),
        Err(_) => return Ok(None),
    };
    if toks.is_empty() {
        return Ok(None);
    }
    if let [Tok::Ident(name)] = toks.as_slice() {
        if !matches!(name.as_str(), "true" | "false" | "null") {
            return Ok(None);
        }
    }
    let mut parser = Parser {
        toks: &toks,
        pos: 0,
        depth: 0,
    };
    let expression = match parser.parse_or() {
        Ok(expression) => expression,
        Err(error)
            if error == "integer literal out of range"
                || error.starts_with("expression nesting")
                || error == "expression exceeds 1024 tokens" =>
        {
            return Err(error)
        }
        Err(_) => return Ok(None),
    };
    if parser.pos != toks.len() {
        return Ok(None);
    }
    if toks.len() > 1024 {
        return Err("expression exceeds 1024 tokens".into());
    }
    Ok(Some(CompiledExpr(Arc::new(expression))))
}

/// Evaluate a closure expression against a borrowed scope. No values are
/// cloned until the expression actually returns or operates on one of them.
pub(crate) fn try_eval_with<'a>(
    source: &str,
    lookup: impl Fn(&str) -> Option<&'a Value>,
) -> Result<Option<Value>, String> {
    compile(source)?
        .map(|expression| expression.evaluate_with(lookup))
        .transpose()
}

/// Compatibility entry point for callers that already own a variable map.
pub fn try_eval(source: &str, vars: &HashMap<String, Value>) -> Result<Option<Value>, String> {
    try_eval_with(source, |name| vars.get(name))
}

fn has_shell_syntax(src: &str) -> bool {
    // Single | is an operator (||) only if doubled. A lone | / ; / ` / & means shell.
    let bytes = src.as_bytes();
    let mut in_str = false;
    let mut i = 0;
    while i < bytes.len() {
        let c = bytes[i];
        if in_str && c != b'"' {
            // An escaped quote is part of the string, not its closing quote.
            if c == b'\\' && i + 1 < bytes.len() {
                i += 2;
            } else {
                i += 1;
            }
            continue;
        }
        if c == b'"' {
            in_str = !in_str;
            i += 1;
            continue;
        }
        match c {
            b';' | b'`' => return true,
            b'|' => {
                if i + 1 < bytes.len() && bytes[i + 1] == b'|' {
                    i += 2;
                    continue;
                }
                return true;
            }
            b'&' => {
                if i + 1 < bytes.len() && bytes[i + 1] == b'&' {
                    i += 2;
                    continue;
                }
                return true;
            }
            _ => i += 1,
        }
    }
    false
}

fn eval<'a>(
    e: &Expr,
    vars: &impl Fn(&str) -> Option<&'a Value>,
    depth: usize,
) -> Result<Value, String> {
    if depth > 128 {
        return Err("expression evaluation exceeds 128 levels".into());
    }
    match e {
        Expr::Literal(value) => Ok(value.clone()),
        Expr::Int(i) => Ok(Value::Int(*i)),
        Expr::Float(f) => Ok(Value::Float(*f)),
        Expr::Str(s) => Ok(Value::String(s.clone())),
        Expr::Bool(b) => Ok(Value::Bool(*b)),
        Expr::Null => Ok(Value::Null),
        Expr::Var(name, path) => Ok(vars(name)
            .and_then(|base| crate::expand::resolve_path(base, path))
            .cloned()
            .unwrap_or(Value::Null)),
        Expr::Neg(x) => match eval(x, vars, depth + 1)? {
            Value::Int(i) => i
                .checked_neg()
                .map(Value::Int)
                .ok_or_else(|| "integer overflow".into()),
            Value::Float(f) => Ok(Value::Float(-f)),
            v => Err(format!("cannot negate {:?}", v)),
        },
        Expr::Not(x) => Ok(Value::Bool(!is_truthy(&eval(x, vars, depth + 1)?))),
        Expr::And(a, b) => {
            let va = eval(a, vars, depth + 1)?;
            if !is_truthy(&va) {
                return Ok(Value::Bool(false));
            }
            Ok(Value::Bool(is_truthy(&eval(b, vars, depth + 1)?)))
        }
        Expr::Or(a, b) => {
            let va = eval(a, vars, depth + 1)?;
            if is_truthy(&va) {
                return Ok(Value::Bool(true));
            }
            Ok(Value::Bool(is_truthy(&eval(b, vars, depth + 1)?)))
        }
        Expr::If(c, t, e) => {
            if is_truthy(&eval(c, vars, depth + 1)?) {
                eval(t, vars, depth + 1)
            } else {
                eval(e, vars, depth + 1)
            }
        }
        Expr::Match(scrut, arms) => {
            let sv = eval(scrut, vars, depth + 1)?;
            for (pat, body) in arms {
                let hit = match pat {
                    MatchPat::Wildcard => true,
                    MatchPat::Lit(v) => &sv == v,
                };
                if hit {
                    return eval(body, vars, depth + 1);
                }
            }
            Ok(Value::Null)
        }
        Expr::Cmp(l, op, r) => {
            let lv = eval(l, vars, depth + 1)?;
            let rv = eval(r, vars, depth + 1)?;
            let res = match op {
                Cmp::Eq => lv == rv,
                Cmp::Ne => lv != rv,
                Cmp::Lt | Cmp::Gt | Cmp::Le | Cmp::Ge => {
                    let order = match (lv.as_number(), rv.as_number()) {
                        (Some(a), Some(b)) => a.compare(b),
                        _ => Some(lv.to_display_string().cmp(&rv.to_display_string())),
                    };
                    match op {
                        Cmp::Lt => order.is_some_and(std::cmp::Ordering::is_lt),
                        Cmp::Gt => order.is_some_and(std::cmp::Ordering::is_gt),
                        Cmp::Le => order.is_some_and(std::cmp::Ordering::is_le),
                        Cmp::Ge => order.is_some_and(std::cmp::Ordering::is_ge),
                        _ => unreachable!(),
                    }
                }
            };
            Ok(Value::Bool(res))
        }
        Expr::Bin(l, op, r) => {
            let lv = eval(l, vars, depth + 1)?;
            let rv = eval(r, vars, depth + 1)?;
            // String concatenation: `+` on any String operand.
            if let Bin::Add = op {
                if matches!(lv, Value::String(_)) || matches!(rv, Value::String(_)) {
                    return Ok(Value::String(format!(
                        "{}{}",
                        lv.to_display_string(),
                        rv.to_display_string()
                    )));
                }
            }
            // Evaluate each operand once, and keep integer operations exact.
            if let (Value::Int(a), Value::Int(b)) = (&lv, &rv) {
                let exact = match op {
                    Bin::Add => a.checked_add(*b),
                    Bin::Sub => a.checked_sub(*b),
                    Bin::Mul => a.checked_mul(*b),
                    Bin::Mod if *b == 0 => return Err("modulo by zero".into()),
                    Bin::Mod if *b == -1 => Some(0),
                    Bin::Mod => Some(a % b),
                    Bin::Div if *b == 0 => return Err("division by zero".into()),
                    Bin::Div => {
                        let quotient = a.checked_div(*b).ok_or("integer overflow")?;
                        if a % b == 0 {
                            return Ok(Value::Int(quotient));
                        }
                        return Ok(Value::Float(*a as f64 / *b as f64));
                    }
                };
                return exact
                    .map(Value::Int)
                    .ok_or_else(|| "integer overflow".into());
            }
            let (a, b) = match (lv.as_f64(), rv.as_f64()) {
                (Some(a), Some(b)) => (a, b),
                _ => return Err(format!("non-numeric in arithmetic: {:?} {:?}", lv, rv)),
            };
            Ok(Value::Float(match op {
                Bin::Add => a + b,
                Bin::Sub => a - b,
                Bin::Mul => a * b,
                Bin::Div => {
                    if b == 0.0 {
                        return Err("division by zero".into());
                    }
                    a / b
                }
                Bin::Mod => {
                    if b == 0.0 {
                        return Err("modulo by zero".into());
                    }
                    a % b
                }
            }))
        }
    }
}

fn is_truthy(v: &Value) -> bool {
    match v {
        Value::Null => false,
        Value::Bool(b) => *b,
        Value::Int(i) => *i != 0,
        Value::Float(f) => *f != 0.0,
        Value::String(s) => !s.is_empty(),
        Value::List(x) => !x.is_empty(),
        Value::Record(m) => !m.is_empty(),
        Value::Binary(b) => !b.is_empty(),
        Value::Closure(_) => true,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::cell::Cell;

    #[test]
    fn cached_expressions_reuse_syntax_and_resolve_each_scope_again() {
        let first = compile("$cache_test_value + 2").unwrap().unwrap();
        let second = compile("$cache_test_value + 2").unwrap().unwrap();
        assert!(Arc::ptr_eq(&first.0, &second.0));
        for value in [Value::Int(3), Value::Int(40)] {
            assert_eq!(
                first.evaluate_with(|_| Some(&value)).unwrap(),
                if value == Value::Int(3) {
                    Value::Int(5)
                } else {
                    Value::Int(42)
                }
            );
        }
    }

    #[test]
    fn every_operand_is_evaluated_once() {
        let reads = Cell::new(0);
        let value = Value::Int(3);
        let expression = compile("($x + 1) * ($x + 2)").unwrap().unwrap();
        assert_eq!(
            expression
                .evaluate_with(|_| {
                    reads.set(reads.get() + 1);
                    Some(&value)
                })
                .unwrap(),
            Value::Int(20)
        );
        assert_eq!(reads.get(), 2);
    }

    #[test]
    fn expression_cache_is_bounded_by_entries_and_source_bytes() {
        let mut cache = ExpressionCache::default();
        for index in 0..256 {
            let source = format!("{index}{}", " ".repeat(4090));
            cache.insert(&source, None);
            assert!(cache.entries.len() <= 128);
            assert!(cache.source_bytes <= 256 * 1024);
        }
        assert_eq!(
            cache.source_bytes,
            cache.entries.keys().map(String::len).sum::<usize>()
        );
        cache.insert(&"x".repeat(8193), None);
        assert!(!cache.entries.contains_key(&"x".repeat(8193)));
    }

    #[test]
    fn excessive_expression_depth_and_size_are_errors() {
        let vars = HashMap::new();
        for source in [
            format!("{}1", "!".repeat(140)),
            std::iter::repeat_n("1", 300)
                .collect::<Vec<_>>()
                .join(" + "),
            std::iter::repeat_n("1", 600)
                .collect::<Vec<_>>()
                .join(" + "),
            std::iter::repeat_n("1", 10_000)
                .collect::<Vec<_>>()
                .join(" + "),
        ] {
            assert!(try_eval(&source, &vars).is_err());
        }
    }

    #[test]
    fn shell_arguments_are_not_subject_to_expression_numeric_or_size_limits() {
        for source in [
            "echo 18446744073709551616".to_owned(),
            "$command 18446744073709551616".to_owned(),
            format!("echo {}", "arg ".repeat(1500)),
            format!("$command {}", "arg ".repeat(1500)),
        ] {
            assert!(matches!(try_eval(&source, &HashMap::new()), Ok(None)));
        }
    }
}
