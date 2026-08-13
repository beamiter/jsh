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
use std::collections::HashMap;

#[derive(Debug, Clone, PartialEq)]
enum Tok {
    Num(f64, bool), // value, is_int
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
                let f: f64 = s.parse().map_err(|_| format!("bad number '{}'", s))?;
                out.push(Tok::Num(f, !is_float));
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
        match self.peek() {
            Some(Tok::Bang) => {
                self.bump();
                let e = self.parse_unary()?;
                Ok(Expr::Not(Box::new(e)))
            }
            Some(Tok::Minus) => {
                self.bump();
                let e = self.parse_unary()?;
                Ok(Expr::Neg(Box::new(e)))
            }
            _ => self.parse_primary(),
        }
    }
    fn parse_primary(&mut self) -> Result<Expr, String> {
        match self.bump() {
            Some(Tok::Num(n, is_int)) => {
                if is_int {
                    Ok(Expr::Int(n as i64))
                } else {
                    Ok(Expr::Float(n))
                }
            }
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
            Some(Tok::Ident(s)) => match s.as_str() {
                "true" => Ok(MatchPat::Lit(Value::Bool(true))),
                "false" => Ok(MatchPat::Lit(Value::Bool(false))),
                "null" => Ok(MatchPat::Lit(Value::Null)),
                _ => Err(format!("match pattern must be literal or _, got {}", s)),
            },
            Some(Tok::Num(n, is_int)) => {
                let n = if negate { -n } else { n };
                if is_int {
                    Ok(MatchPat::Lit(Value::Int(n as i64)))
                } else {
                    Ok(MatchPat::Lit(Value::Float(n)))
                }
            }
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

/// Try to evaluate `src` as a closure-body expression against `vars`.
/// Returns Ok(Some(value)) on success, Ok(None) if the body is not a pure
/// expression (so the caller can fall back to the command interpreter), or
/// Err(msg) on a real parse/eval error in an expression-looking body.
pub fn try_eval(src: &str, vars: &HashMap<String, Value>) -> Result<Option<Value>, String> {
    // Cheap heuristic: if the body contains shell-only syntax like `|`, `;`,
    // backtick, or unquoted command-looking tokens, bail out so the command
    // interpreter handles it. We only handle pure expressions here.
    if has_shell_syntax(src) {
        return Ok(None);
    }
    let toks = match tokenize(src.trim()) {
        Ok(t) => t,
        Err(_) => return Ok(None),
    };
    if toks.is_empty() {
        return Ok(None);
    }
    // Reject token sequences that don't look expression-y (e.g. just an Ident
    // that isn't true/false/null — those are commands to run).
    if toks.len() == 1 {
        if let Tok::Ident(s) = &toks[0] {
            if !matches!(s.as_str(), "true" | "false" | "null") {
                return Ok(None);
            }
        }
    }
    let mut p = Parser {
        toks: &toks,
        pos: 0,
    };
    let expr = match p.parse_or() {
        Ok(e) => e,
        Err(_) => return Ok(None),
    };
    if p.pos != toks.len() {
        return Ok(None);
    }
    let v = eval(&expr, vars)?;
    Ok(Some(v))
}

fn has_shell_syntax(src: &str) -> bool {
    // Single | is an operator (||) only if doubled. A lone | / ; / ` / & means shell.
    let bytes = src.as_bytes();
    let mut in_str = false;
    let mut i = 0;
    while i < bytes.len() {
        let c = bytes[i];
        if c == b'"' {
            in_str = !in_str;
            i += 1;
            continue;
        }
        if in_str {
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

fn eval(e: &Expr, vars: &HashMap<String, Value>) -> Result<Value, String> {
    match e {
        Expr::Int(i) => Ok(Value::Int(*i)),
        Expr::Float(f) => Ok(Value::Float(*f)),
        Expr::Str(s) => Ok(Value::String(s.clone())),
        Expr::Bool(b) => Ok(Value::Bool(*b)),
        Expr::Null => Ok(Value::Null),
        Expr::Var(name, path) => {
            let base = vars.get(name).cloned().unwrap_or(Value::Null);
            Ok(crate::expand::resolve_path(&base, path)
                .cloned()
                .unwrap_or(Value::Null))
        }
        Expr::Neg(x) => match eval(x, vars)? {
            Value::Int(i) => Ok(Value::Int(-i)),
            Value::Float(f) => Ok(Value::Float(-f)),
            v => Err(format!("cannot negate {:?}", v)),
        },
        Expr::Not(x) => Ok(Value::Bool(!is_truthy(&eval(x, vars)?))),
        Expr::And(a, b) => {
            let va = eval(a, vars)?;
            if !is_truthy(&va) {
                return Ok(Value::Bool(false));
            }
            Ok(Value::Bool(is_truthy(&eval(b, vars)?)))
        }
        Expr::Or(a, b) => {
            let va = eval(a, vars)?;
            if is_truthy(&va) {
                return Ok(Value::Bool(true));
            }
            Ok(Value::Bool(is_truthy(&eval(b, vars)?)))
        }
        Expr::If(c, t, e) => {
            if is_truthy(&eval(c, vars)?) {
                eval(t, vars)
            } else {
                eval(e, vars)
            }
        }
        Expr::Match(scrut, arms) => {
            let sv = eval(scrut, vars)?;
            for (pat, body) in arms {
                let hit = match pat {
                    MatchPat::Wildcard => true,
                    MatchPat::Lit(v) => &sv == v,
                };
                if hit {
                    return eval(body, vars);
                }
            }
            Ok(Value::Null)
        }
        Expr::Cmp(l, op, r) => {
            let lv = eval(l, vars)?;
            let rv = eval(r, vars)?;
            let res = match op {
                Cmp::Eq => lv == rv,
                Cmp::Ne => lv != rv,
                Cmp::Lt | Cmp::Gt | Cmp::Le | Cmp::Ge => {
                    match (lv.as_f64(), rv.as_f64()) {
                        (Some(a), Some(b)) => match op {
                            Cmp::Lt => a < b,
                            Cmp::Gt => a > b,
                            Cmp::Le => a <= b,
                            Cmp::Ge => a >= b,
                            _ => unreachable!(),
                        },
                        _ => {
                            // Fall back to string compare for ordering of strings.
                            let a = lv.to_display_string();
                            let b = rv.to_display_string();
                            match op {
                                Cmp::Lt => a < b,
                                Cmp::Gt => a > b,
                                Cmp::Le => a <= b,
                                Cmp::Ge => a >= b,
                                _ => unreachable!(),
                            }
                        }
                    }
                }
            };
            Ok(Value::Bool(res))
        }
        Expr::Bin(l, op, r) => {
            let lv = eval(l, vars)?;
            let rv = eval(r, vars)?;
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
            let (a, b) = match (lv.as_f64(), rv.as_f64()) {
                (Some(a), Some(b)) => (a, b),
                _ => return Err(format!("non-numeric in arithmetic: {:?} {:?}", lv, rv)),
            };
            let both_int =
                matches!(eval(l, vars)?, Value::Int(_)) && matches!(eval(r, vars)?, Value::Int(_));
            let f = match op {
                Bin::Add => a + b,
                Bin::Sub => a - b,
                Bin::Mul => a * b,
                Bin::Div => {
                    if b == 0.0 {
                        return Err("division by zero".to_string());
                    }
                    a / b
                }
                Bin::Mod => {
                    if b == 0.0 {
                        return Err("modulo by zero".to_string());
                    }
                    a % b
                }
            };
            if both_int && matches!(op, Bin::Add | Bin::Sub | Bin::Mul | Bin::Mod) {
                Ok(Value::Int(f as i64))
            } else if both_int && matches!(op, Bin::Div) && (a as i64) % (b as i64) == 0 {
                Ok(Value::Int((a as i64) / (b as i64)))
            } else {
                Ok(Value::Float(f))
            }
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
