//! `${...}` parameter expansion: the parameter forms (`name`, `name[sub]`,
//! positional and special parameters, `${#...}`, `${!...}`) and every
//! operator bash applies to them, element by element for `@`/`*` lists.

use crate::environment::ShellState;
use crate::parser::ast::{Word, WordPart};
use crate::variables::{
    array_assignment_body, double_quote_value, quote_assoc_key, single_quote_value, VarValue,
    Variable,
};

use super::{
    expand_arithmetic, expand_pattern_word, expand_subscript_text, expand_word_to_string,
    ifs_first, parameter_value,
};

/// What a `${...}` expansion produced.
pub(super) enum ParamResult {
    /// An unset parameter: no text at all.
    Unset,
    Scalar(String),
    /// One value per element: `$@`, `${a[@]}`, `${!a[@]}`, `${@/x/y}`, ...
    /// `star` marks the `*` forms, which a quoted context joins with the
    /// first character of IFS.
    List {
        items: Vec<String>,
        star: bool,
    },
    /// A `${x:-word}`-style operand, still to be expanded in the caller's
    /// quoting context: `${x:-"a b"}` is one field even unquoted.
    Word(Word),
}

impl ParamResult {
    /// The single string the expansion yields where words are not split
    /// (assignments, `[[ ]]`, here-documents).
    pub(super) fn into_string(self, state: &mut ShellState) -> String {
        match self {
            ParamResult::Unset => String::new(),
            ParamResult::Scalar(s) => s,
            ParamResult::List { items, star } => {
                let sep = if star {
                    ifs_first(state)
                } else {
                    " ".to_string()
                };
                items.join(&sep)
            }
            ParamResult::Word(word) => expand_word_to_string(&word, state),
        }
    }
}

/// The parameter a `${...}` body names, before any operator.
#[derive(Debug, Clone, PartialEq, Eq)]
enum Form<'a> {
    /// `${name...}`
    Plain,
    /// `${#name}`: length.
    Length,
    /// `${!name...}`: the value of `name` names the parameter to expand.
    Indirect,
    /// `${!name[@]}` / `${!name[*]}`: the array's subscripts.
    Keys,
    /// `${!prefix@}` / `${!prefix*}`: names of variables starting with prefix.
    Names { prefix: &'a str },
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct Spec<'a> {
    form: Form<'a>,
    name: &'a str,
    subscript: Option<&'a str>,
    /// Everything after the parameter: the operator and its operand.
    op: &'a str,
}

/// Split `name[subscript]rest` / `digits rest` / `special rest`.
fn split_param(body: &str) -> Option<(&str, Option<&str>, &str)> {
    let b = body.as_bytes();
    let first = *b.first()?;
    let end = match first {
        b'_' | b'A'..=b'Z' | b'a'..=b'z' => b
            .iter()
            .take_while(|c| **c == b'_' || c.is_ascii_alphanumeric())
            .count(),
        b'0'..=b'9' => b.iter().take_while(|c| c.is_ascii_digit()).count(),
        b'@' | b'*' | b'#' | b'?' | b'-' | b'$' | b'!' => 1,
        _ => return None,
    };
    let name = &body[..end];
    let identifier = first == b'_' || first.is_ascii_alphabetic();
    if identifier && b.get(end) == Some(&b'[') {
        let mut depth = 0usize;
        let mut i = end;
        loop {
            match b.get(i)? {
                b'[' => depth += 1,
                b']' => {
                    depth -= 1;
                    if depth == 0 {
                        break;
                    }
                }
                _ => {}
            }
            i += 1;
        }
        return Some((name, Some(&body[end + 1..i]), &body[i + 1..]));
    }
    Some((name, None, &body[end..]))
}

fn is_identifier_prefix(text: &str) -> bool {
    crate::environment::is_valid_identifier(text)
}

fn parse_spec(body: &str) -> Option<Spec<'_>> {
    if body.len() > 1 {
        if let Some(rest) = body.strip_prefix('#') {
            if let Some((name, subscript, "")) = split_param(rest) {
                return Some(Spec {
                    form: Form::Length,
                    name,
                    subscript,
                    op: "",
                });
            }
            // Otherwise `#` is the parameter itself: `${#:-0}`.
        }
        if let Some(rest) = body.strip_prefix('!') {
            if let Some(prefix) = rest.strip_suffix('@').or_else(|| rest.strip_suffix('*')) {
                if is_identifier_prefix(prefix) {
                    return Some(Spec {
                        form: Form::Names { prefix },
                        name: "",
                        subscript: None,
                        op: &rest[rest.len() - 1..],
                    });
                }
            }
            let (name, subscript, op) = split_param(rest)?;
            let form = if matches!(subscript, Some("@" | "*")) && op.is_empty() {
                Form::Keys
            } else {
                Form::Indirect
            };
            return Some(Spec {
                form,
                name,
                subscript,
                op,
            });
        }
    }
    let (name, subscript, op) = split_param(body)?;
    Some(Spec {
        form: Form::Plain,
        name,
        subscript,
        op,
    })
}

/// A list-valued parameter and where its elements came from.
#[derive(Debug, Clone)]
struct List {
    items: Vec<String>,
    star: bool,
    /// The subscript of each element of an indexed array (slicing is by
    /// index there); `None` for other lists.
    indices: Option<Vec<i64>>,
    /// `$@`/`$*`, whose slices count `$0` as element 0.
    positional: bool,
}

/// A parameter's value before its operator is applied.
#[derive(Debug, Clone)]
enum Base {
    Unset,
    Scalar(String),
    List(List),
}

impl Base {
    fn is_set(&self) -> bool {
        match self {
            Base::Unset => false,
            Base::Scalar(_) => true,
            Base::List(list) => !list.items.is_empty(),
        }
    }

    /// The colon operators treat a null value like an unset one. A list is
    /// null when it would expand to the empty string as `"$*"`.
    fn is_null(&self) -> bool {
        match self {
            Base::Unset => true,
            Base::Scalar(s) => s.is_empty(),
            Base::List(list) => {
                list.items.is_empty() || (list.items.len() == 1 && list.items[0].is_empty())
            }
        }
    }

    fn into_result(self) -> ParamResult {
        match self {
            Base::Unset => ParamResult::Unset,
            Base::Scalar(s) => ParamResult::Scalar(s),
            Base::List(list) => ParamResult::List {
                items: list.items,
                star: list.star,
            },
        }
    }

    /// Apply `f` to the value, or to every element of a list.
    fn map(self, mut f: impl FnMut(&str) -> String) -> Base {
        match self {
            Base::Unset => Base::Unset,
            Base::Scalar(s) => Base::Scalar(f(&s)),
            Base::List(mut list) => {
                list.items = list.items.iter().map(|s| f(s)).collect();
                Base::List(list)
            }
        }
    }
}

fn bad_substitution(body: &str, state: &mut ShellState) -> ParamResult {
    state.expansion_error = Some(format!("${{{}}}: bad substitution", body));
    ParamResult::Unset
}

/// Expand the body of `${...}` (or of `$name`).
pub(super) fn expand_param(body: &str, state: &mut ShellState) -> ParamResult {
    let Some(spec) = parse_spec(body) else {
        return bad_substitution(body, state);
    };
    let base = match &spec.form {
        Form::Names { prefix } => {
            let star = spec.op == "*";
            let mut names: Vec<String> = state
                .visible_variables()
                .into_iter()
                .filter(|(name, _, var)| {
                    name.starts_with(prefix)
                        && crate::environment::is_valid_identifier(name)
                        && (var.is_set() || var.is_array())
                })
                .map(|(name, _, _)| name.to_string())
                .collect();
            for dynamic in [
                "RANDOM",
                "SRANDOM",
                "SECONDS",
                "EPOCHSECONDS",
                "EPOCHREALTIME",
                "BASHPID",
            ] {
                if dynamic.starts_with(prefix) && state.is_dynamic_var(dynamic) {
                    names.push(dynamic.to_string());
                }
            }
            names.sort();
            names.dedup();
            return ParamResult::List { items: names, star };
        }
        Form::Keys => {
            let target = resolved_name(spec.name, state);
            let items = match state.find_variable(&target).map(|v| &v.value) {
                Some(VarValue::Indexed(a)) => a.indices().map(|i| i.to_string()).collect(),
                Some(VarValue::Assoc(m)) => m.keys(),
                Some(VarValue::Scalar(_)) => vec!["0".to_string()],
                _ => Vec::new(),
            };
            return ParamResult::List {
                items,
                star: spec.subscript == Some("*"),
            };
        }
        Form::Indirect => match indirect_base(&spec, state) {
            Ok(base) => base,
            Err(message) => {
                state.expansion_error = Some(message);
                return ParamResult::Unset;
            }
        },
        // `${#a[@]}` is asked in loop conditions; count without copying.
        Form::Length if matches!(spec.subscript, Some("@" | "*")) => {
            let target = resolved_name(spec.name, state);
            let count = match state.find_variable(&target).map(|v| &v.value) {
                Some(VarValue::Indexed(a)) => a.len(),
                Some(VarValue::Assoc(m)) => m.len(),
                Some(VarValue::Scalar(_)) => 1,
                _ => 0,
            };
            return ParamResult::Scalar(count.to_string());
        }
        Form::Plain | Form::Length => match base_value(spec.name, spec.subscript, state) {
            Ok(base) => base,
            Err(message) => {
                state.expansion_error = Some(message);
                return ParamResult::Unset;
            }
        },
    };

    if state.shell_opts.nounset && matches!(base, Base::Unset) && !is_default_operator(spec.op) {
        // `set -u`: the plain `$name`/`${name}` forms are checked before the
        // command runs; everything else that reaches an unset parameter is
        // checked here (`${a[3]}`, `${#x}`, `${x#pat}`, `${!ref}`).
        let simple = spec.form == Form::Plain && spec.subscript.is_none() && spec.op.is_empty();
        if !simple && !state.let_vars.contains_key(spec.name) {
            let what = match spec.subscript {
                Some(sub) => format!("{}[{}]", spec.name, sub),
                None if spec.name.as_bytes()[0].is_ascii_digit() => format!("${}", spec.name),
                None => spec.name.to_string(),
            };
            state.expansion_error = Some(format!("{what}: unbound variable"));
            return ParamResult::Unset;
        }
    }

    if spec.form == Form::Length {
        return ParamResult::Scalar(match base {
            Base::Unset => "0".to_string(),
            Base::Scalar(s) => s.chars().count().to_string(),
            Base::List(list) => list.items.len().to_string(),
        });
    }

    apply_operator(body, &spec, base, state)
}

/// Is `op` one of `-`, `=`, `?`, `+` (with or without a colon), which are
/// defined for unset parameters?
fn is_default_operator(op: &str) -> bool {
    op.strip_prefix(':')
        .unwrap_or(op)
        .starts_with(['-', '=', '?', '+'])
}

/// Follow a nameref for an array reference; element namerefs keep their name.
fn resolved_name(name: &str, state: &ShellState) -> String {
    match state.resolve_nameref(name) {
        Ok(target) if target.subscript.is_none() => target.name,
        _ => name.to_string(),
    }
}

fn positional_list(star: bool, state: &ShellState) -> Base {
    Base::List(List {
        items: state.positional_params.clone(),
        star,
        indices: None,
        positional: true,
    })
}

/// The value of parameter `name` (with an optional subscript).
fn base_value(name: &str, subscript: Option<&str>, state: &mut ShellState) -> Result<Base, String> {
    let Some(subscript) = subscript else {
        return Ok(match name {
            "@" | "*" => positional_list(name == "*", state),
            _ => match parameter_value(name, state) {
                Some(value) => Base::Scalar(value),
                None => Base::Unset,
            },
        });
    };
    let target = state
        .resolve_nameref(name)
        .map_err(|message| message.to_string())?;
    let name = target.name;
    if subscript == "@" || subscript == "*" {
        let star = subscript == "*";
        let var = state.find_variable(&name);
        let (items, indices) = match var.map(|v| &v.value) {
            Some(VarValue::Indexed(a)) => {
                (a.values().cloned().collect(), Some(a.indices().collect()))
            }
            Some(VarValue::Assoc(m)) => (m.values(), None),
            Some(VarValue::Scalar(s)) => (vec![s.clone()], Some(vec![0])),
            _ => (Vec::new(), Some(Vec::new())),
        };
        return Ok(Base::List(List {
            items,
            star,
            indices,
            positional: false,
        }));
    }
    element_value(&name, subscript, state)
}

/// `${name[subscript]}`.
fn element_value(name: &str, subscript: &str, state: &mut ShellState) -> Result<Base, String> {
    let assoc = state.find_variable(name).is_some_and(Variable::is_assoc);
    let key = expand_subscript_text(subscript, state);
    if assoc {
        return Ok(match state.get_array_element(name, &key) {
            Some(value) => Base::Scalar(value),
            None => Base::Unset,
        });
    }
    // The subscript is an arithmetic expression. A malformed one is an
    // expansion error, fatal for the command as in bash.
    let index = match key.trim().parse::<i64>() {
        Ok(n) => n,
        Err(_) => {
            let value = expand_arithmetic(&key, state);
            if state.expansion_error.is_some() {
                return Ok(Base::Unset);
            }
            value.trim().parse().unwrap_or(0)
        }
    };
    let value = match state.find_variable(name).map(|v| &v.value) {
        Some(VarValue::Indexed(a)) => match a.resolve(index) {
            Some(i) => a.get(i).map(str::to_string),
            None => {
                // Bash reports a negative subscript before the start, but
                // the expansion carries on, empty.
                eprintln!("jsh: {name}: bad array subscript");
                None
            }
        },
        Some(VarValue::Scalar(s)) if index == 0 || index == -1 => Some(s.clone()),
        Some(VarValue::Unset) | None if index == 0 => {
            // `${RANDOM[0]}` and friends: a dynamic scalar is element 0.
            state.var_value(name).map(|v| v.into_owned())
        }
        _ => None,
    };
    Ok(match value {
        Some(value) => Base::Scalar(value),
        None => Base::Unset,
    })
}

/// `${!name...}`: expand the parameter whose name is the value of `name`.
/// A nameref yields the name it refers to instead.
fn indirect_base(spec: &Spec<'_>, state: &mut ShellState) -> Result<Base, String> {
    if spec.subscript.is_none() {
        if let Some(var) = state.find_variable(spec.name) {
            if var.attrs.contains(crate::variables::VarAttrs::NAMEREF) {
                return Ok(match &var.value {
                    VarValue::Scalar(target) => Base::Scalar(target.clone()),
                    _ => Base::Unset,
                });
            }
        }
    }
    let reference = match base_value(spec.name, spec.subscript, state)? {
        Base::Scalar(s) => s,
        Base::Unset => return Ok(Base::Unset),
        Base::List(list) => list.items.join(" "),
    };
    match split_param(&reference) {
        Some((name, subscript, "")) => base_value(name, subscript, state),
        _ => Err(format!("{reference}: invalid variable name")),
    }
}

/// Apply the operator in `spec.op` to `base`.
fn apply_operator(body: &str, spec: &Spec<'_>, base: Base, state: &mut ShellState) -> ParamResult {
    let op = spec.op;
    if op.is_empty() {
        return base.into_result();
    }

    // Default-value family: `-`, `=`, `?`, `+`, each with an optional colon.
    let (colon, rest) = match op.strip_prefix(':') {
        Some(rest) if rest.starts_with(['-', '=', '?', '+']) => (true, rest),
        _ => (false, op),
    };
    if let Some(kind) = rest
        .chars()
        .next()
        .filter(|c| matches!(c, '-' | '=' | '?' | '+'))
    {
        let operand = &rest[1..];
        let missing = if colon {
            base.is_null()
        } else {
            !base.is_set()
        };
        return match kind {
            '-' => {
                if missing {
                    ParamResult::Word(crate::parser::parse_word_parts(operand))
                } else {
                    base.into_result()
                }
            }
            '+' => {
                if missing {
                    // An absent list stays a list: `"${a[@]+x}"` on an empty
                    // array is no field at all, like `"${a[@]}"`.
                    match base {
                        Base::List(list) => ParamResult::List {
                            items: Vec::new(),
                            star: list.star,
                        },
                        _ => ParamResult::Unset,
                    }
                } else {
                    ParamResult::Word(crate::parser::parse_word_parts(operand))
                }
            }
            '=' => {
                if !missing {
                    return base.into_result();
                }
                assign_default(spec, operand, state)
            }
            _ => {
                if !missing {
                    return base.into_result();
                }
                let message =
                    expand_word_to_string(&crate::parser::parse_word_parts(operand), state);
                let what = match spec.subscript {
                    Some(sub) => format!("{}[{}]", spec.name, sub),
                    None => spec.name.to_string(),
                };
                state.expansion_error = Some(if message.is_empty() {
                    if colon {
                        format!("{what}: parameter null or not set")
                    } else {
                        format!("{what}: parameter not set")
                    }
                } else {
                    format!("{what}: {message}")
                });
                ParamResult::Unset
            }
        };
    }

    let first = op.as_bytes()[0];
    match first {
        b'#' | b'%' => {
            let longest = op.len() > 1 && op.as_bytes()[1] == first;
            let pattern = &op[if longest { 2 } else { 1 }..];
            let pattern = expand_pattern_text(pattern, state);
            let options = match_options(state, false);
            let prefix = first == b'#';
            base.map(|value| strip_pattern(value, &pattern, prefix, longest, options))
                .into_result()
        }
        b'/' => {
            let replace = Replacement::parse(&op[1..], state);
            let options = match_options(state, true);
            base.map(|value| replace.apply(value, options))
                .into_result()
        }
        b'^' | b',' | b'~' => {
            let all = op.len() > 1 && op.as_bytes()[1] == first;
            let pattern = &op[if all { 2 } else { 1 }..];
            let pattern = if pattern.is_empty() {
                None
            } else {
                Some(expand_pattern_text(pattern, state))
            };
            let options = match_options(state, false);
            base.map(|value| convert_case(value, first, all, pattern.as_deref(), options))
                .into_result()
        }
        b':' => substring(&op[1..], base, state),
        b'@' if op.len() == 2 => transform(spec, op.as_bytes()[1], base, state),
        _ => bad_substitution(body, state),
    }
}

/// `${name=word}` / `${name:=word}`: assign the expanded word, then expand
/// to the variable's (attribute-processed) value.
fn assign_default(spec: &Spec<'_>, operand: &str, state: &mut ShellState) -> ParamResult {
    let value = expand_word_to_string(&crate::parser::parse_word_parts(operand), state);
    let assignable = spec.form == Form::Plain && crate::environment::is_valid_identifier(spec.name);
    if !assignable || matches!(spec.subscript, Some("@" | "*")) {
        state.expansion_error = Some(format!("${}: cannot assign in this way", spec.name));
        return ParamResult::Unset;
    }
    match spec.subscript {
        Some(subscript) => {
            let key = expand_subscript_text(subscript, state);
            state.assign_element(spec.name, &key, &value, false);
            match element_value(spec.name, subscript, state) {
                Ok(base) => base.into_result(),
                Err(_) => ParamResult::Scalar(value),
            }
        }
        None => {
            state.set_var(spec.name, &value);
            ParamResult::Scalar(state.get_var(spec.name).unwrap_or(&value).to_string())
        }
    }
}

fn match_options(state: &ShellState, honor_nocasematch: bool) -> crate::glob_match::MatchOptions {
    crate::glob_match::MatchOptions {
        extglob: state.shell_opts.extglob,
        nocase: honor_nocasematch && nocasematch(state),
    }
}

/// Is `shopt -s nocasematch` on?
pub(crate) fn nocasematch(state: &ShellState) -> bool {
    state
        .shell_opts
        .shopt_opts
        .get("nocasematch")
        .copied()
        .unwrap_or(false)
}

/// The pattern operand of `#`, `%`, `/` and the case operators, with quoted
/// characters escaped so they match literally.
fn expand_pattern_text(raw: &str, state: &mut ShellState) -> String {
    expand_pattern_word(&crate::parser::parse_word_parts(raw), state)
}

fn matches(pattern: &str, text: &str, options: crate::glob_match::MatchOptions) -> bool {
    crate::glob_match::pattern_match_with(pattern, text, options)
}

/// The text a pattern matches when it has no pattern characters at all, with
/// its escapes removed. Such patterns (`${v//-/_}`, `${p%.txt}`) are the
/// common case and are answered by plain string search instead of trying a
/// glob match at every pair of positions.
fn literal_pattern(pattern: &str, options: crate::glob_match::MatchOptions) -> Option<String> {
    if options.nocase {
        return None;
    }
    let mut literal = String::with_capacity(pattern.len());
    let mut chars = pattern.chars();
    while let Some(c) = chars.next() {
        match c {
            '\\' => literal.push(chars.next()?),
            '*' | '?' | '[' => return None,
            '(' if options.extglob => return None,
            _ => literal.push(c),
        }
    }
    Some(literal)
}

/// `${v#pat}`, `${v##pat}`, `${v%pat}`, `${v%%pat}`.
fn strip_pattern(
    value: &str,
    pattern: &str,
    prefix: bool,
    longest: bool,
    options: crate::glob_match::MatchOptions,
) -> String {
    if let Some(literal) = literal_pattern(pattern, options) {
        let stripped = if prefix {
            value.strip_prefix(literal.as_str())
        } else {
            value.strip_suffix(literal.as_str())
        };
        return stripped.unwrap_or(value).to_string();
    }
    let compiled = crate::glob_match::CompiledPattern::new(pattern, options);
    let chars = compiled.prepare(value);
    let bounds = char_bounds(value);
    let n = chars.len();
    // Candidate cut points, in the order the operator prefers them.
    let cuts: Vec<usize> = match compiled.fixed_len {
        Some(len) if len <= n => vec![if prefix { len } else { n - len }],
        Some(_) => Vec::new(),
        None => match (prefix, longest) {
            (true, false) => (0..=n).collect(),
            (true, true) => (0..=n).rev().collect(),
            (false, false) => (0..=n).rev().collect(),
            (false, true) => (0..=n).collect(),
        },
    };
    for cut in cuts {
        let candidate = if prefix { &chars[..cut] } else { &chars[cut..] };
        if compiled.matches(candidate) {
            return if prefix {
                value[bounds[cut]..].to_string()
            } else {
                value[..bounds[cut]].to_string()
            };
        }
    }
    value.to_string()
}

/// Byte offset of every character boundary of `value`, end included.
fn char_bounds(value: &str) -> Vec<usize> {
    value
        .char_indices()
        .map(|(i, _)| i)
        .chain(std::iter::once(value.len()))
        .collect()
}

/// A parsed `/pattern/replacement` operand.
struct Replacement {
    pattern: String,
    /// The replacement split at each unquoted `&`, which (with bash's
    /// default `patsub_replacement`) stands for the matched text.
    pieces: Vec<String>,
    global: bool,
    anchor: Anchor,
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum Anchor {
    None,
    Start,
    End,
}

impl Replacement {
    fn parse(spec: &str, state: &mut ShellState) -> Replacement {
        let (global, spec) = match spec.strip_prefix('/') {
            Some(rest) => (true, rest),
            None => (false, spec),
        };
        let (anchor, spec) = if let Some(rest) = spec.strip_prefix('#') {
            (Anchor::Start, rest)
        } else if let Some(rest) = spec.strip_prefix('%') {
            (Anchor::End, rest)
        } else {
            (Anchor::None, spec)
        };
        let (pattern, replacement) = split_at_unquoted_slash(spec);
        let pattern = expand_pattern_text(pattern, state);
        let pieces = split_at_unquoted_ampersand(replacement)
            .into_iter()
            .map(|piece| expand_word_to_string(&crate::parser::parse_word_parts(&piece), state))
            .collect();
        Replacement {
            pattern,
            pieces,
            global,
            anchor,
        }
    }

    fn render(&self, matched: &str) -> String {
        self.pieces.join(matched)
    }

    /// [`Self::apply`] for a pattern that matches only `literal`.
    fn apply_literal(&self, value: &str, literal: &str) -> String {
        let replacement = self.render(literal);
        match self.anchor {
            Anchor::Start => match value.strip_prefix(literal) {
                Some(rest) => format!("{replacement}{rest}"),
                None => value.to_string(),
            },
            Anchor::End => match value.strip_suffix(literal) {
                Some(rest) => format!("{rest}{replacement}"),
                None => value.to_string(),
            },
            Anchor::None if self.global => value.replace(literal, &replacement),
            Anchor::None => value.replacen(literal, &replacement, 1),
        }
    }

    fn apply(&self, value: &str, options: crate::glob_match::MatchOptions) -> String {
        if self.pattern.is_empty() {
            // An empty pattern matches only where it is anchored.
            return match self.anchor {
                Anchor::Start => format!("{}{}", self.render(""), value),
                Anchor::End => format!("{}{}", value, self.render("")),
                Anchor::None => value.to_string(),
            };
        }
        if let Some(literal) = literal_pattern(&self.pattern, options) {
            return self.apply_literal(value, &literal);
        }
        let compiled = crate::glob_match::CompiledPattern::new(&self.pattern, options);
        let chars = compiled.prepare(value);
        let bounds = char_bounds(value);
        let n = chars.len();
        // Ends to try for a match starting at `start`, longest first.
        let ends = |start: usize| -> Vec<usize> {
            match compiled.fixed_len {
                Some(len) if start + len <= n => vec![start + len],
                Some(_) => Vec::new(),
                None => (start..=n).rev().collect(),
            }
        };
        match self.anchor {
            Anchor::Start => {
                for end in ends(0) {
                    if compiled.matches(&chars[..end]) {
                        let matched = &value[..bounds[end]];
                        return format!("{}{}", self.render(matched), &value[bounds[end]..]);
                    }
                }
                value.to_string()
            }
            Anchor::End => {
                let starts: Vec<usize> = match compiled.fixed_len {
                    Some(len) if len <= n => vec![n - len],
                    Some(_) => Vec::new(),
                    None => (0..=n).collect(),
                };
                for start in starts {
                    if compiled.matches(&chars[start..]) {
                        let matched = &value[bounds[start]..];
                        return format!("{}{}", &value[..bounds[start]], self.render(matched));
                    }
                }
                value.to_string()
            }
            Anchor::None => {
                // Most values contain no match at all; settle that with one
                // scan before trying every start and end.
                let anywhere = crate::glob_match::CompiledPattern::new(
                    &format!("*{}*", self.pattern),
                    options,
                );
                if !anywhere.matches(&chars) {
                    return value.to_string();
                }
                let first = compiled.first_literal();
                let mut out = String::new();
                let mut i = 0;
                let mut replaced = false;
                while i < n {
                    let found =
                        if (replaced && !self.global) || first.is_some_and(|c| chars[i] != c) {
                            None
                        } else {
                            // The longest non-empty match starting here.
                            ends(i)
                                .into_iter()
                                .filter(|end| *end > i)
                                .find(|end| compiled.matches(&chars[i..*end]))
                        };
                    match found {
                        Some(end) => {
                            out.push_str(&self.render(&value[bounds[i]..bounds[end]]));
                            i = end;
                            replaced = true;
                        }
                        None => {
                            out.push_str(&value[bounds[i]..bounds[i + 1]]);
                            i += 1;
                        }
                    }
                }
                out
            }
        }
    }
}

/// Split `pat/rep` at the first `/` that is not escaped or quoted.
fn split_at_unquoted_slash(spec: &str) -> (&str, &str) {
    let mut quote: Option<char> = None;
    let mut chars = spec.char_indices();
    while let Some((i, c)) = chars.next() {
        match (quote, c) {
            (None, '\\') => {
                chars.next();
            }
            (Some('"'), '\\') => {
                chars.next();
            }
            (None, '\'' | '"') => quote = Some(c),
            (Some(q), c) if c == q => quote = None,
            (None, '/') => return (&spec[..i], &spec[i + 1..]),
            _ => {}
        }
    }
    (spec, "")
}

/// Split a replacement at every unquoted, unescaped `&`. A `\&` becomes a
/// literal `&` in the piece it belongs to.
fn split_at_unquoted_ampersand(rep: &str) -> Vec<String> {
    let mut pieces = vec![String::new()];
    let mut quote: Option<char> = None;
    let mut chars = rep.chars().peekable();
    while let Some(c) = chars.next() {
        let piece = pieces.last_mut().expect("never empty");
        match (quote, c) {
            (None, '\\') => match chars.next() {
                // The backslash only protected the ampersand; the piece is
                // re-parsed, so keep the escape for anything else.
                Some('&') => piece.push('&'),
                Some(other) => {
                    piece.push('\\');
                    piece.push(other);
                }
                None => piece.push('\\'),
            },
            (Some('"'), '\\') => {
                piece.push('\\');
                if let Some(next) = chars.next() {
                    piece.push(next);
                }
            }
            (None, '\'' | '"') => {
                quote = Some(c);
                piece.push(c);
            }
            (Some(q), c) if c == q => {
                quote = None;
                piece.push(c);
            }
            (None, '&') => pieces.push(String::new()),
            _ => piece.push(c),
        }
    }
    pieces
}

/// `${v^pat}`, `${v^^pat}`, `${v,pat}`, `${v,,pat}`, `${v~pat}`, `${v~~pat}`.
/// The pattern is matched against each character; bash defaults it to `?`.
fn convert_case(
    value: &str,
    op: u8,
    all: bool,
    pattern: Option<&str>,
    options: crate::glob_match::MatchOptions,
) -> String {
    let mut out = String::with_capacity(value.len());
    for (idx, c) in value.chars().enumerate() {
        let eligible = (all || idx == 0)
            && pattern.is_none_or(|p| matches(p, c.encode_utf8(&mut [0u8; 4]), options));
        if !eligible {
            out.push(c);
            continue;
        }
        match op {
            b'^' => out.extend(c.to_uppercase()),
            b',' => out.extend(c.to_lowercase()),
            _ => {
                if c.is_uppercase() {
                    out.extend(c.to_lowercase());
                } else {
                    out.extend(c.to_uppercase());
                }
            }
        }
    }
    out
}

/// Split `offset:length` at the colon that is not part of a `?:` ternary.
fn split_offset_length(text: &str) -> (&str, Option<&str>) {
    let mut ternary = 0usize;
    let mut depth = 0usize;
    for (i, c) in text.char_indices() {
        match c {
            '(' | '[' => depth += 1,
            ')' | ']' => depth = depth.saturating_sub(1),
            '?' if depth == 0 => ternary += 1,
            ':' if depth == 0 => {
                if ternary > 0 {
                    ternary -= 1;
                } else {
                    return (&text[..i], Some(&text[i + 1..]));
                }
            }
            _ => {}
        }
    }
    (text, None)
}

/// Expand and evaluate an offset or length; `None` after an error.
fn eval_arith_operand(text: &str, state: &mut ShellState) -> Option<i64> {
    let expanded = expand_subscript_text(text, state);
    if expanded.trim().is_empty() {
        return Some(0);
    }
    let value = expand_arithmetic(&expanded, state);
    if state.expansion_error.is_some() {
        return None;
    }
    Some(value.trim().parse().unwrap_or(0))
}

/// `${v:offset}` / `${v:offset:length}`: substring of a scalar, slice of a
/// list. Offset and length are arithmetic expressions.
fn substring(spec: &str, base: Base, state: &mut ShellState) -> ParamResult {
    let (offset_text, length_text) = split_offset_length(spec);
    let Some(offset) = eval_arith_operand(offset_text, state) else {
        return ParamResult::Unset;
    };
    let length = match length_text {
        Some(text) => match eval_arith_operand(text, state) {
            Some(n) => Some(n),
            None => return ParamResult::Unset,
        },
        None => None,
    };
    match base {
        Base::Unset => ParamResult::Unset,
        Base::Scalar(value) => {
            let bounds: Vec<usize> = value
                .char_indices()
                .map(|(i, _)| i)
                .chain(std::iter::once(value.len()))
                .collect();
            let len = (bounds.len() - 1) as i64;
            let start = if offset < 0 {
                len.saturating_add(offset)
            } else {
                offset
            };
            if start < 0 || start > len {
                return ParamResult::Scalar(String::new());
            }
            let end = match length {
                None => len,
                Some(n) if n < 0 => {
                    let end = len.saturating_add(n);
                    if end < start {
                        state.expansion_error = Some(format!(
                            "{}: substring expression < 0",
                            length_text.unwrap_or("").trim()
                        ));
                        return ParamResult::Unset;
                    }
                    end
                }
                Some(n) => start.saturating_add(n).min(len),
            };
            ParamResult::Scalar(value[bounds[start as usize]..bounds[end as usize]].to_string())
        }
        Base::List(list) => {
            if length.is_some_and(|n| n < 0) {
                state.expansion_error = Some(format!(
                    "{}: substring expression < 0",
                    length_text.unwrap_or("").trim()
                ));
                return ParamResult::Unset;
            }
            let star = list.star;
            let items = slice_list(list, offset, length, state);
            ParamResult::List { items, star }
        }
    }
}

fn slice_list(list: List, offset: i64, length: Option<i64>, state: &ShellState) -> Vec<String> {
    let take = |items: Vec<String>| -> Vec<String> {
        match length {
            Some(n) => items.into_iter().take(n.max(0) as usize).collect(),
            None => items,
        }
    };
    if list.positional {
        // `$0` is element 0 of `$@` for slicing.
        let mut all = Vec::with_capacity(list.items.len() + 1);
        all.push(state.arg0.clone());
        all.extend(list.items);
        let len = all.len() as i64;
        let start = if offset < 0 {
            len.saturating_add(offset)
        } else {
            offset
        };
        if start < 0 || start > len {
            return Vec::new();
        }
        return take(all.into_iter().skip(start as usize).collect());
    }
    match list.indices {
        Some(indices) => {
            // Indexed arrays slice by subscript: the elements at or after
            // `offset`, which counts back from one past the last index.
            let end = indices.last().map_or(0, |max| max.saturating_add(1));
            let start = if offset < 0 {
                end.saturating_add(offset)
            } else {
                offset
            };
            if start < 0 {
                return Vec::new();
            }
            take(
                indices
                    .into_iter()
                    .zip(list.items)
                    .filter(|(i, _)| *i >= start)
                    .map(|(_, v)| v)
                    .collect(),
            )
        }
        None => {
            let len = list.items.len() as i64;
            let start = if offset < 0 {
                len.saturating_add(offset)
            } else {
                offset
            };
            if start < 0 || start > len {
                return Vec::new();
            }
            take(list.items.into_iter().skip(start as usize).collect())
        }
    }
}

/// `${v@op}`.
fn transform(spec: &Spec<'_>, op: u8, base: Base, state: &mut ShellState) -> ParamResult {
    match op {
        b'U' => base.map(|v| v.to_uppercase()).into_result(),
        b'L' => base.map(|v| v.to_lowercase()).into_result(),
        b'u' => base
            .map(|v| {
                let mut chars = v.chars();
                match chars.next() {
                    Some(first) => first.to_uppercase().chain(chars).collect(),
                    None => String::new(),
                }
            })
            .into_result(),
        b'Q' => match base {
            Base::Unset => ParamResult::Unset,
            other => other.map(single_quote_value).into_result(),
        },
        b'E' => base.map(crate::parser::parse::decode_ansi_c).into_result(),
        b'P' => base.map(|v| expand_prompt(v, state)).into_result(),
        b'a' => {
            let letters = attribute_letters(spec, state);
            match base {
                Base::List(list) => ParamResult::List {
                    items: vec![letters; list.items.len()],
                    star: list.star,
                },
                _ if letters.is_empty() && !state.is_var_set(spec.name) => ParamResult::Unset,
                _ => ParamResult::Scalar(letters),
            }
        }
        b'A' => assignment_form(spec, base, state),
        b'K' | b'k' => key_value_form(spec, op == b'K', base, state),
        _ => bad_substitution(&format!("{}@{}", spec.name, op as char), state),
    }
}

/// The variable a transformation describes (namerefs followed).
fn described_variable(spec: &Spec<'_>, state: &ShellState) -> Option<(String, Variable)> {
    if matches!(spec.name, "@" | "*" | "#" | "?" | "-" | "$" | "!")
        || spec.name.as_bytes()[0].is_ascii_digit()
    {
        return None;
    }
    let name = resolved_name(spec.name, state);
    let var = state.find_variable(&name)?.clone();
    Some((name, var))
}

fn attribute_letters(spec: &Spec<'_>, state: &ShellState) -> String {
    described_variable(spec, state)
        .map(|(_, var)| var.attr_letters())
        .unwrap_or_default()
}

/// `${v@A}`: an assignment (or `declare`) statement that recreates `v`.
fn assignment_form(spec: &Spec<'_>, base: Base, state: &mut ShellState) -> ParamResult {
    let Some((name, var)) = described_variable(spec, state) else {
        return match base {
            // `${@@A}` describes the positional parameters as a `set` command.
            Base::List(list) if list.positional => {
                let mut items = vec!["set".to_string(), "--".to_string()];
                items.extend(list.items.iter().map(|v| single_quote_value(v)));
                ParamResult::List {
                    items,
                    star: list.star,
                }
            }
            _ => ParamResult::Unset,
        };
    };
    let letters = var.attr_letters();
    let flags = if letters.is_empty() {
        String::new()
    } else {
        format!("declare -{letters} ")
    };
    match base {
        Base::List(list) => {
            // `${arr[@]@A}` is the `declare` command for the whole array, as
            // three words.
            let assignment = if var.is_set() {
                format!("{name}={}", array_assignment_body(&var.value))
            } else {
                name.clone()
            };
            let flag_word = if letters.is_empty() {
                "--".to_string()
            } else {
                format!("-{letters}")
            };
            ParamResult::List {
                items: vec!["declare".to_string(), flag_word, assignment],
                star: list.star,
            }
        }
        Base::Scalar(value) => {
            ParamResult::Scalar(format!("{flags}{name}={}", single_quote_value(&value)))
        }
        Base::Unset => {
            if letters.is_empty() {
                ParamResult::Unset
            } else {
                ParamResult::Scalar(format!("declare -{letters} {name}"))
            }
        }
    }
}

/// `${a[@]@K}` (one word of `key "value"` pairs) and `${a[@]@k}` (keys and
/// values as separate words). On a scalar both quote the value like `@Q`.
fn key_value_form(
    spec: &Spec<'_>,
    quoted_pairs: bool,
    base: Base,
    state: &mut ShellState,
) -> ParamResult {
    let Base::List(list) = base else {
        return match base {
            Base::Unset => ParamResult::Unset,
            other => other.map(single_quote_value).into_result(),
        };
    };
    let pairs: Vec<(String, String)> = match described_variable(spec, state).map(|(_, v)| v.value) {
        Some(VarValue::Indexed(a)) => a.iter().map(|(i, v)| (i.to_string(), v.clone())).collect(),
        Some(VarValue::Assoc(m)) => m
            .iter()
            .map(|(k, v)| (k.to_string(), v.to_string()))
            .collect(),
        _ => list
            .items
            .iter()
            .enumerate()
            .map(|(i, v)| ((i + 1).to_string(), v.clone()))
            .collect(),
    };
    let assoc = state.is_assoc(spec.name);
    if quoted_pairs {
        let text = if assoc {
            pairs
                .iter()
                .map(|(k, v)| format!("{} {} ", quote_assoc_key(k), double_quote_value(v)))
                .collect::<String>()
        } else {
            pairs
                .iter()
                .map(|(k, v)| format!("{} {}", k, double_quote_value(v)))
                .collect::<Vec<_>>()
                .join(" ")
        };
        return ParamResult::Scalar(text);
    }
    ParamResult::List {
        items: pairs.into_iter().flat_map(|(k, v)| [k, v]).collect(),
        star: list.star,
    }
}

/// `${v@P}`: expand the prompt escapes bash's `PS1` understands.
fn expand_prompt(text: &str, state: &ShellState) -> String {
    let mut out = String::new();
    let mut chars = text.chars();
    while let Some(c) = chars.next() {
        if c != '\\' {
            out.push(c);
            continue;
        }
        let Some(code) = chars.next() else {
            out.push('\\');
            break;
        };
        match code {
            'u' => out.push_str(&std::env::var("USER").unwrap_or_else(|_| {
                nix::unistd::User::from_uid(nix::unistd::getuid())
                    .ok()
                    .flatten()
                    .map(|u| u.name)
                    .unwrap_or_default()
            })),
            'h' => out.push_str(state.hostname.split('.').next().unwrap_or("")),
            'H' => out.push_str(&state.hostname),
            'w' | 'W' => {
                let cwd = std::env::current_dir()
                    .map(|p| p.to_string_lossy().into_owned())
                    .unwrap_or_default();
                let home = state.get_var("HOME").unwrap_or("").to_string();
                let shown = if code == 'W' {
                    if cwd == home {
                        "~".to_string()
                    } else {
                        cwd.rsplit('/')
                            .next()
                            .filter(|s| !s.is_empty())
                            .unwrap_or("/")
                            .to_string()
                    }
                } else if !home.is_empty() && (cwd == home || cwd.starts_with(&format!("{home}/")))
                {
                    format!("~{}", &cwd[home.len()..])
                } else {
                    cwd
                };
                out.push_str(&shown);
            }
            '$' => out.push(if nix::unistd::geteuid().is_root() {
                '#'
            } else {
                '$'
            }),
            's' => out.push_str(
                std::path::Path::new(&state.arg0)
                    .file_name()
                    .and_then(|n| n.to_str())
                    .unwrap_or("jsh"),
            ),
            'n' => out.push('\n'),
            'r' => out.push('\r'),
            'a' => out.push('\x07'),
            'e' => out.push('\x1b'),
            '\\' => out.push('\\'),
            '[' | ']' => {}
            other => {
                out.push('\\');
                out.push(other);
            }
        }
    }
    out
}

/// Is this `${...}` body a plain parameter that [`super::expand_variable`]
/// already answers (`name`, `1`, `?`), rather than a form this module owns?
pub(super) fn is_simple_parameter(body: &str) -> bool {
    match split_param(body) {
        Some((name, None, "")) => name != "@" && name != "*",
        _ => false,
    }
}

/// Does `word` start with a `[key]=` subscript, as an element of a compound
/// assignment can? Returns the raw key, whether it is `+=`, and the value's
/// parts.
pub(super) fn keyed_element(word: &Word) -> Option<(String, bool, Word)> {
    let (WordPart::Glob(head), rest) = word.split_first()? else {
        return None;
    };
    let key = head.strip_prefix('[')?.strip_suffix(']')?;
    let (first_rest, tail) = rest.split_first()?;
    let WordPart::Literal(text) = first_rest else {
        return None;
    };
    let (append, value_start) = if let Some(v) = text.strip_prefix("+=") {
        (true, v)
    } else {
        (false, text.strip_prefix('=')?)
    };
    let mut value: Word = Vec::with_capacity(tail.len() + 1);
    if !value_start.is_empty() {
        // Literal text is final: its escapes were already resolved.
        value.push(WordPart::Literal(value_start.to_string()));
    }
    value.extend(tail.iter().cloned());
    Some((key.to_string(), append, value))
}
