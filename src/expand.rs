/// Variable, tilde, glob, command substitution, arithmetic, array, and process
/// substitution expansion.
use crate::environment::ShellState;
use crate::parser::ast::{InterpPart, PathSeg, ProcessSubKind, Word, WordPart};
use crate::value::Value;

/// Expand a Word (`Vec<WordPart>`) into a list of strings.
/// Word splitting and globbing may produce multiple strings from one Word.
pub fn expand_word(word: &Word, state: &mut ShellState) -> Vec<String> {
    // Array expansions produce one field per element: ${arr[@]} and ${arr[*]},
    // quoted or not (only "${arr[*]}" joins into a single field).
    if word_refs_array_at_star(word, state) {
        let fields = expand_parts_to_fields(word, state, false);
        // "${empty[@]}" contributes no fields at all, the same way "$@" doesn't.
        if fields.len() == 1 && fields[0].is_empty() && array_refs_all_empty(word, state) {
            return Vec::new();
        }
        return glob_fields(fields, state);
    }

    // Check for "$@"/$@/$* which expand to multiple fields (positional params).
    if word_contains_at_star(word) {
        let fields = expand_parts_to_fields(word, state, false);
        // "$@" / "$*" with no positional params and no surrounding text → no words.
        if fields.len() == 1 && fields[0].is_empty() && state.positional_params.is_empty() {
            return Vec::new();
        }
        return glob_fields(fields, state);
    }

    // Check for brace expansion/range that produces multiple words.
    //
    // Brace expansion runs before pathname expansion, and each alternative keeps
    // the quoting it was written with: `{*,zz}` globs while `{'*',zz}` does not.
    // Splicing an alternative's *parts* back into the word preserves that;
    // substituting its expanded text would flatten `'*'` and `*` to the same
    // thing. Ranges are generated text, so they become plain literals.
    for (i, part) in word.iter().enumerate() {
        let alternatives: Option<Vec<Word>> = match part {
            WordPart::BraceExpansion(items) => Some(items.clone()),
            WordPart::BraceRange { start, end, step } => Some(
                expand_brace_range(start, end, step.as_deref())
                    .into_iter()
                    .map(|item| vec![WordPart::Literal(item)])
                    .collect(),
            ),
            _ => None,
        };
        if let Some(alternatives) = alternatives {
            let mut results = Vec::new();
            for alternative in &alternatives {
                let mut new_word: Word = Vec::new();
                for (j, p) in word.iter().enumerate() {
                    if j == i {
                        new_word.extend(alternative.iter().cloned());
                    } else {
                        new_word.push(p.clone());
                    }
                }
                results.extend(expand_word(&new_word, state));
            }
            return results;
        }
    }

    // Expand parts while tracking, per byte, which bytes came from unquoted
    // expansions (candidates for IFS word splitting) and which may act as glob
    // metacharacters (candidates for pathname expansion).
    let (expanded, mask, has_unsplittable) = expand_word_masked(word, state);

    let mut fields = ifs_split(&expanded, &mask, state);
    if fields.is_empty() {
        if has_unsplittable {
            // e.g. `echo ""` or `echo "$empty"` → one empty field.
            fields.push(Field::new());
        } else {
            // e.g. `echo $empty` → no fields at all.
            return Vec::new();
        }
    }

    // Globbing applies per resulting field, after word splitting.
    glob_fields(fields, state)
}

/// One field of an expanded word, carrying per-byte glob eligibility alongside
/// its text.
///
/// Quoting **cannot** be recovered from expanded text: the parser has already
/// consumed the quote characters, so any `'` or `"` still present arrived as
/// *data*. Re-deriving it by scanning (which this module used to do) was wrong
/// in both directions — `echo '*'` globbed, and a filename containing an
/// apostrophe silently disabled globbing for its whole field.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
struct Field {
    text: String,
    /// `globbable[i]` is true when byte `i` of `text` may act as a glob
    /// metacharacter. Always the same length as `text`.
    globbable: Vec<bool>,
}

impl Field {
    fn new() -> Self {
        Self::default()
    }

    /// Append `text`, marking every one of its bytes with the same eligibility.
    fn push_str(&mut self, text: &str, globbable: bool) {
        self.text.push_str(text);
        self.globbable.resize(self.text.len(), globbable);
    }

    /// Append one character, marking each of its UTF-8 bytes.
    fn push_char(&mut self, c: char, globbable: bool) {
        self.text.push(c);
        self.globbable.resize(self.text.len(), globbable);
    }

    fn is_empty(&self) -> bool {
        self.text.is_empty()
    }

    /// A field whose bytes are all glob-eligible — used where the caller already
    /// knows the text came from an unquoted expansion.
    fn unquoted(text: &str) -> Self {
        let mut field = Self::new();
        field.push_str(text, true);
        field
    }
}

/// Expand each part of a word, recording per-byte whether it originated from an
/// unquoted expansion (splittable) plus whether any unsplittable part was seen
/// (so an empty result can be distinguished: `echo ""` keeps one empty field,
/// `echo $empty` keeps none).
fn expand_word_masked(word: &Word, state: &mut ShellState) -> (Field, Vec<bool>, bool) {
    let mut field = Field::new();
    let mut mask: Vec<bool> = Vec::new();
    let mut has_unsplittable = false;
    for part in word {
        let splittable = matches!(
            part,
            WordPart::Variable(_) | WordPart::CommandSub(_) | WordPart::Arithmetic(_)
        );
        if !splittable {
            has_unsplittable = true;
        }
        let text = expand_part(part, state);
        for _ in 0..text.len() {
            mask.push(splittable);
        }
        field.push_str(&text, part_is_globbable(part, false));
    }
    (field, mask, has_unsplittable)
}

/// May the bytes this part expands to act as glob metacharacters?
///
/// Only two sources qualify, matching POSIX: metacharacters the *parser* saw
/// unquoted (`WordPart::Glob`, which is the only place unquoted `*?[` land), and
/// the results of unquoted expansions (`x='*'; echo $x` does glob). Everything
/// else is protected — quoted text, backslash escapes (which the parser folds
/// into `Literal`), tilde expansion, and process substitution paths. `quoted`
/// is set when the part sits inside double quotes, which protects it outright.
fn part_is_globbable(part: &WordPart, quoted: bool) -> bool {
    if quoted {
        return false;
    }
    matches!(
        part,
        WordPart::Glob(_)
            | WordPart::Variable(_)
            | WordPart::VariablePath { .. }
            | WordPart::CommandSub(_)
            | WordPart::Arithmetic(_)
    )
}

/// Split a string into fields at splittable IFS characters. `mask[byte]` is true
/// where the byte came from an unquoted expansion. Honors bash IFS rules:
/// leading/trailing IFS whitespace is trimmed, runs of IFS whitespace delimit a
/// single field boundary, and each IFS non-whitespace character (with any
/// adjacent IFS whitespace) is one delimiter that can produce empty fields.
fn ifs_split(input: &Field, mask: &[bool], state: &ShellState) -> Vec<Field> {
    let s = input.text.as_str();
    if s.is_empty() {
        return Vec::new();
    }
    let ifs = state
        .get_var("IFS")
        .map(|v| v.to_string())
        .unwrap_or_else(|| " \t\n".to_string());
    if ifs.is_empty() {
        // IFS empty → no word splitting at all.
        return vec![input.clone()];
    }
    let is_ws = |c: char| (c == ' ' || c == '\t' || c == '\n') && ifs.contains(c);
    let is_ifs = |c: char| ifs.contains(c);

    let chars: Vec<(usize, char)> = s.char_indices().collect();
    let n = chars.len();
    let split_at = |i: usize| -> bool {
        let bp = chars[i].0;
        mask.get(bp).copied().unwrap_or(false) && is_ifs(chars[i].1)
    };

    let mut fields: Vec<Field> = Vec::new();
    let mut field = Field::new();
    let mut field_pending = false;
    let mut i = 0;

    // Trim leading IFS whitespace.
    while i < n && split_at(i) && is_ws(chars[i].1) {
        i += 1;
    }

    while i < n {
        if split_at(i) {
            let c = chars[i].1;
            if is_ws(c) {
                // Consume the whitespace run, then optionally one non-ws
                // delimiter plus its trailing whitespace.
                let mut j = i;
                while j < n && split_at(j) && is_ws(chars[j].1) {
                    j += 1;
                }
                let mut took_nonws = false;
                if j < n && split_at(j) && !is_ws(chars[j].1) {
                    j += 1;
                    took_nonws = true;
                    while j < n && split_at(j) && is_ws(chars[j].1) {
                        j += 1;
                    }
                }
                if j < n || took_nonws {
                    fields.push(std::mem::take(&mut field));
                    field_pending = false;
                }
                // else: trailing whitespace — ignore.
                i = j;
            } else {
                // Non-whitespace IFS delimiter.
                fields.push(std::mem::take(&mut field));
                field_pending = false;
                i += 1;
                while i < n && split_at(i) && is_ws(chars[i].1) {
                    i += 1;
                }
            }
        } else {
            let (byte_pos, c) = chars[i];
            let globbable = input.globbable.get(byte_pos).copied().unwrap_or(false);
            field.push_char(c, globbable);
            field_pending = true;
            i += 1;
        }
    }

    if field_pending || !field.is_empty() {
        fields.push(field);
    }
    fields
}

/// IFS-split a string that came entirely from an unquoted expansion. Such bytes
/// are glob-eligible: `arr=('*'); echo ${arr[@]}` globs, exactly as bash does.
fn ifs_split_unquoted(s: &str, state: &ShellState) -> Vec<Field> {
    let mask = vec![true; s.len()];
    ifs_split(&Field::unquoted(s), &mask, state)
}

/// Apply glob expansion to every field of a split word, in order.
fn glob_fields(fields: Vec<Field>, state: &mut ShellState) -> Vec<String> {
    let mut out = Vec::new();
    for field in fields {
        out.extend(glob_field(&field, state));
    }
    out
}

/// Turn a field into a glob pattern, neutralising every metacharacter that
/// quoting protected so the matcher sees it as literal text.
///
/// Escaping is delegated to the `glob` crate so it always agrees with the
/// matcher that consumes the result (it wraps a metacharacter in a one-element
/// character class: `*` → `[*]`).
fn glob_pattern_for(field: &Field) -> String {
    let mut pattern = String::with_capacity(field.text.len());
    for (byte_pos, c) in field.text.char_indices() {
        if field.globbable.get(byte_pos).copied().unwrap_or(false) {
            pattern.push(c);
        } else {
            pattern.push_str(&glob::Pattern::escape(&c.to_string()));
        }
    }
    pattern
}

/// Apply glob/extglob expansion to a single already-split field.
fn glob_field(field: &Field, state: &mut ShellState) -> Vec<String> {
    let expanded = field.text.as_str();
    let has_extglob = state.shell_opts.extglob && crate::glob_match::contains_extglob(expanded);

    if has_extglob {
        expand_with_extglob(expanded, state)
    } else if field_contains_glob(field) && !state.shell_opts.noglob {
        let pattern = glob_pattern_for(field);
        match glob::glob(&pattern) {
            Ok(paths) => {
                let mut results: Vec<String> = paths
                    .filter_map(|p| p.ok())
                    .map(|p| p.to_string_lossy().to_string())
                    .collect();

                // Apply dotglob filtering: remove hidden files if dotglob is off and pattern doesn't explicitly match them
                if !state.shell_opts.dotglob && !pattern_explicitly_includes_dot(expanded) {
                    results.retain(|path| {
                        let filename = std::path::Path::new(path)
                            .file_name()
                            .and_then(|n| n.to_str())
                            .unwrap_or("");
                        !filename.starts_with('.')
                    });
                }

                if results.is_empty() {
                    if state.shell_opts.failglob {
                        state.expansion_error = Some(format!("no match: {}", expanded));
                        vec![]
                    } else if state.shell_opts.nullglob {
                        vec![]
                    } else {
                        vec![expanded.to_string()]
                    }
                } else {
                    results.sort();
                    results
                }
            }
            Err(_) if state.shell_opts.failglob => {
                state.expansion_error = Some(format!("no match: {}", expanded));
                vec![]
            }
            Err(_) => vec![expanded.to_string()],
        }
    } else {
        vec![expanded.to_string()]
    }
}

/// Split a `name[subscript]` body into its two halves, or `None` when the body
/// is not a closed subscript reference at all.
///
/// Never slice a `${...}` body by hand: `${a[}` arrives here as the name `a[`,
/// where the old `&name[bracket + 1..name.len() - 1]` is the reversed byte
/// range `2..1` and panics — a typo at the prompt killed the whole shell.
fn split_subscript(name: &str) -> Option<(&str, &str)> {
    let bracket = name.find('[')?;
    let close = name.len().checked_sub(1)?;
    if close <= bracket || name.as_bytes()[close] != b']' {
        return None;
    }
    Some((&name[..bracket], &name[bracket + 1..close]))
}

/// Is `s` a plain parameter name (`[A-Za-z_][A-Za-z0-9_]*`)? Used to keep the
/// subscript handling from claiming bodies like `v#[a-z]`, where the `[` opens a
/// glob bracket expression inside a pattern operator instead of a subscript.
fn is_name(s: &str) -> bool {
    let mut bytes = s.bytes();
    match bytes.next() {
        Some(b'_') | Some(b'A'..=b'Z') | Some(b'a'..=b'z') => {}
        _ => return false,
    }
    bytes.all(|b| b == b'_' || b.is_ascii_alphanumeric())
}

/// Does bash reject this `${...}` body outright as a bad substitution? An array
/// subscript that is never closed (`${a[}`, `${a[0}`) or is empty (`${a[]}`) is
/// a fatal error with status 1, not a silently empty expansion — and the
/// unclosed forms are exactly the ones that used to panic the process.
fn subscript_is_malformed(body: &str) -> bool {
    let b = body.as_bytes();
    // `#` (length) and `!` (indirection/keys) are prefixes, not part of the
    // name; the subscript behind them is validated the same way.
    let mut i = match b.first() {
        Some(b'#') | Some(b'!') => 1,
        _ => 0,
    };
    // Only a plain name can carry a subscript. Anything else — `$1`, `$@`, or a
    // pattern operator such as `${v#[a-z]}` — is none of this check's business.
    if !matches!(b.get(i), Some(b'_') | Some(b'A'..=b'Z') | Some(b'a'..=b'z')) {
        return false;
    }
    while i < b.len() && (b[i] == b'_' || b[i].is_ascii_alphanumeric()) {
        i += 1;
    }
    if b.get(i) != Some(&b'[') {
        return false;
    }
    let subscript_start = i + 1;
    let mut depth = 0usize;
    while i < b.len() {
        match b[i] {
            b'[' => depth += 1,
            b']' => {
                depth -= 1;
                if depth == 0 {
                    // `${a[]}`: the brackets close, but with nothing between.
                    return i == subscript_start;
                }
            }
            _ => {}
        }
        i += 1;
    }
    true // ran off the end of the body with the `[` still open
}

/// The values `var_name` contributes through an `[@]`/`[*]` subscript. Bash
/// treats a plain scalar as a one-element array, so `${x[0]}`, `${x[@]}` and
/// `${#x[@]}` all answer for `x=abc`; jsh used to expand them to nothing.
fn element_values(var_name: &str, state: &ShellState) -> Vec<String> {
    if state.is_array(var_name) {
        state.array_values(var_name)
    } else {
        match state.get_var(var_name) {
            Some(v) => vec![v.to_string()],
            None => Vec::new(),
        }
    }
}

/// One element of `var_name[subscript]`, treating a scalar as a one-element
/// array so that `${x[0]}` on `x=abc` yields `abc` the way bash does. The
/// subscript is expanded and, for an indexed array, evaluated arithmetically
/// (`${a[i+1]}`); a failure leaves an expansion error and yields `None`.
fn subscript_element(var_name: &str, subscript: &str, state: &mut ShellState) -> Option<String> {
    let key = resolve_array_subscript(var_name, subscript, state)?;
    if state.is_array(var_name) {
        return state.get_array_element(var_name, &key);
    }
    let value = state.get_var(var_name)?;
    (key == "0").then(|| value.to_string())
}

/// Split `arr[@]` / `arr[*]` into (name, subscript) when the name has elements
/// to expand: a declared array, or a scalar standing in for a one-element one.
fn array_at_star_ref<'a>(name: &'a str, state: &ShellState) -> Option<(&'a str, &'a str)> {
    let (var_name, subscript) = split_subscript(name)?;
    if (subscript == "@" || subscript == "*")
        && (state.is_array(var_name) || state.get_var(var_name).is_some())
    {
        Some((var_name, subscript))
    } else {
        None
    }
}

/// Does this word reference an array as `${arr[@]}` / `${arr[*]}` (at top level
/// or inside double quotes)?
fn word_refs_array_at_star(word: &Word, state: &ShellState) -> bool {
    fn walk(part: &WordPart, state: &ShellState) -> bool {
        match part {
            WordPart::Variable(name) => array_at_star_ref(name, state).is_some(),
            WordPart::DoubleQuoted(parts) => parts.iter().any(|p| walk(p, state)),
            _ => false,
        }
    }
    word.iter().any(|p| walk(p, state))
}

/// Are all the arrays this word references via `[@]`/`[*]` empty?
fn array_refs_all_empty(word: &Word, state: &ShellState) -> bool {
    fn walk(part: &WordPart, state: &ShellState) -> bool {
        match part {
            WordPart::Variable(name) => match array_at_star_ref(name, state) {
                Some((var_name, _)) => element_values(var_name, state).is_empty(),
                None => true,
            },
            WordPart::DoubleQuoted(parts) => parts.iter().all(|p| walk(p, state)),
            _ => true,
        }
    }
    word.iter().all(|p| walk(p, state))
}

/// Does this word reference $@ or $* (top-level or inside double quotes)?
fn word_contains_at_star(word: &Word) -> bool {
    word.iter().any(part_refs_at_star)
}

fn part_refs_at_star(part: &WordPart) -> bool {
    match part {
        WordPart::Variable(name) => name == "@" || name == "*",
        WordPart::DoubleQuoted(parts) => parts.iter().any(part_refs_at_star),
        _ => false,
    }
}

/// First character of IFS (used to join "$*"). Default is a space; an empty IFS
/// joins with no separator.
fn ifs_first(state: &ShellState) -> String {
    match state.get_var("IFS") {
        Some(s) => s.chars().next().map(|c| c.to_string()).unwrap_or_default(),
        None => " ".to_string(),
    }
}

/// Expand a list of parts into separate fields, honoring $@/$* splitting rules.
/// `quoted` indicates the parts are inside double quotes (affects $* joining).
///
/// `quoted` also decides glob eligibility for the values this produces:
/// positional parameters and array elements are patterns only while unquoted, so
/// `set -- '*'; echo $@` globs while `echo "$@"` does not.
fn expand_parts_to_fields(parts: &[WordPart], state: &mut ShellState, quoted: bool) -> Vec<Field> {
    let mut fields: Vec<Field> = vec![Field::new()];
    for part in parts {
        match part {
            WordPart::Variable(name) if name == "@" => {
                let params = state.positional_params.clone();
                append_fields(&mut fields, as_fields(params, !quoted));
            }
            WordPart::Variable(name) if name == "*" => {
                if quoted {
                    let sep = ifs_first(state);
                    let joined = state.positional_params.join(&sep);
                    fields.last_mut().unwrap().push_str(&joined, false);
                } else {
                    let params = state.positional_params.clone();
                    append_fields(&mut fields, as_fields(params, !quoted));
                }
            }
            WordPart::Variable(name) if array_at_star_ref(name, state).is_some() => {
                let (var_name, subscript) = array_at_star_ref(name, state).unwrap();
                let vals = element_values(var_name, state);
                if quoted {
                    if subscript == "*" {
                        let sep = ifs_first(state);
                        fields.last_mut().unwrap().push_str(&vals.join(&sep), false);
                    } else {
                        append_fields(&mut fields, as_fields(vals, false));
                    }
                } else {
                    // Unquoted, each element is still subject to IFS splitting.
                    let mut split = Vec::new();
                    for v in &vals {
                        split.extend(ifs_split_unquoted(v, state));
                    }
                    append_fields(&mut fields, split);
                }
            }
            WordPart::DoubleQuoted(inner) => {
                let sub = expand_parts_to_fields(inner, state, true);
                append_fields(&mut fields, sub);
            }
            other => {
                let s = expand_part(other, state);
                let globbable = part_is_globbable(other, quoted);
                fields.last_mut().unwrap().push_str(&s, globbable);
            }
        }
    }
    fields
}

/// Wrap already-expanded strings as fields with uniform glob eligibility.
fn as_fields(values: Vec<String>, globbable: bool) -> Vec<Field> {
    values
        .into_iter()
        .map(|value| {
            let mut field = Field::new();
            field.push_str(&value, globbable);
            field
        })
        .collect()
}

/// Merge additional fields: the first attaches to the current trailing field,
/// the rest become new fields. Empty input leaves `fields` untouched.
fn append_fields(fields: &mut Vec<Field>, more: Vec<Field>) {
    let mut iter = more.into_iter();
    if let Some(first) = iter.next() {
        let last = fields.last_mut().unwrap();
        last.text.push_str(&first.text);
        last.globbable.extend_from_slice(&first.globbable);
        for f in iter {
            fields.push(f);
        }
    }
}

/// Expand a Word into a single string (no word splitting/globbing).
pub fn expand_word_to_string(word: &Word, state: &mut ShellState) -> String {
    let mut result = String::new();
    for part in word {
        result.push_str(&expand_part(part, state));
    }
    result
}

fn expand_part(part: &WordPart, state: &mut ShellState) -> String {
    match part {
        WordPart::Literal(s) => s.clone(),
        WordPart::SingleQuoted(s) => s.clone(),
        WordPart::DoubleQuoted(parts) => {
            let mut s = String::new();
            for p in parts {
                s.push_str(&expand_part(p, state));
            }
            s
        }
        WordPart::Variable(name) => expand_variable(name, state),
        WordPart::Tilde(user) => expand_tilde(user, state),
        WordPart::Glob(pattern) => pattern.clone(), // returned as-is; expanded at Word level
        WordPart::CommandSub(cmd) => expand_command_sub(cmd, state),
        WordPart::Arithmetic(expr) => expand_arithmetic(expr, state),
        WordPart::BraceExpansion(items) => expand_brace_items(items, state).join(" "),
        WordPart::BraceRange { start, end, step } => {
            expand_brace_range(start, end, step.as_deref()).join(" ")
        }
        WordPart::ProcessSub(cmd, kind) => expand_process_sub(cmd, kind, state),
        WordPart::VariablePath { name, path } => expand_variable_path(name, path, state),
        WordPart::Interpolated(parts) => expand_interpolated(parts, state),
        WordPart::Closure { params, body_src } => {
            // Stash a fresh ClosureData (snapshotting let_vars) and return a
            // sentinel string. Closure-aware builtins (each/where) look the
            // closure back up via `state.inline_closures`.
            use crate::value::ClosureData;
            use std::sync::Arc;
            let data = Arc::new(ClosureData {
                params: params.clone(),
                body_src: body_src.clone(),
                captured: state.let_vars.clone(),
            });
            state.inline_closures.push(data);
            format!("\x01jsh-closure:{}\x02", state.inline_closures.len() - 1)
        }
    }
}

/// Render `.field[3].other` etc. as a literal string — used when no typed
/// Value backs the variable (preserves bash `$name.txt` behavior).
fn render_path_as_literal(path: &[PathSeg]) -> String {
    let mut out = String::new();
    for seg in path {
        match seg {
            PathSeg::Field(f) => {
                out.push('.');
                out.push_str(f);
            }
            PathSeg::Index(i) => {
                out.push('[');
                out.push_str(&i.to_string());
                out.push(']');
            }
        }
    }
    out
}

/// Walk a path into a Value. Returns None if any segment doesn't exist.
/// Negative indices count from the end (nushell-style).
pub fn resolve_path<'a>(v: &'a Value, path: &[PathSeg]) -> Option<&'a Value> {
    let mut cur = v;
    for seg in path {
        cur = match (cur, seg) {
            (Value::Record(r), PathSeg::Field(name)) => r.get(name)?,
            (Value::List(items), PathSeg::Index(i)) => {
                let len = items.len() as i64;
                let idx = if *i < 0 { len + *i } else { *i };
                if idx < 0 || idx >= len {
                    return None;
                }
                &items[idx as usize]
            }
            (Value::Record(r), PathSeg::Index(i)) => {
                // Numeric index into a record selects the Nth entry (insertion order).
                let len = r.len() as i64;
                let idx = if *i < 0 { len + *i } else { *i };
                if idx < 0 || idx >= len {
                    return None;
                }
                let (_, v) = r.get_index(idx as usize)?;
                v
            }
            _ => return None,
        };
    }
    Some(cur)
}

fn expand_variable_path(name: &str, path: &[PathSeg], state: &mut ShellState) -> String {
    if let Some(v) = state.let_vars.get(name) {
        if let Some(found) = resolve_path(v, path) {
            return found.to_display_string();
        }
        // Path didn't resolve into the typed value — fall through to literal
        // rendering so the user sees something useful rather than an empty string.
    }
    let mut s = expand_variable(name, state);
    s.push_str(&render_path_as_literal(path));
    s
}

fn expand_interpolated(parts: &[InterpPart], state: &mut ShellState) -> String {
    let mut out = String::new();
    for p in parts {
        match p {
            InterpPart::Lit(s) => out.push_str(s),
            InterpPart::Expr(w) => out.push_str(&expand_word_to_string(w, state)),
        }
    }
    out
}

fn expand_param_operand(raw: &str, state: &mut ShellState) -> String {
    let word = crate::parser::parse_word_parts(raw);
    expand_word_to_string(&word, state)
}

fn parameter_value(name: &str, state: &ShellState) -> Option<String> {
    match name {
        "?" => Some(state.last_exit_code.to_string()),
        "$" => Some(std::process::id().to_string()),
        "!" => Some(state.last_bg_pid.map_or(String::new(), |p| p.to_string())),
        "#" => Some(state.positional_params.len().to_string()),
        "@" | "*" => Some(state.positional_params.join(" ")),
        "0" => Some(state.arg0.clone()),
        "-" => Some(state.option_flags()),
        _ if name.len() <= 3 && name.chars().all(|c| c.is_ascii_digit()) => {
            let idx: usize = name.parse().unwrap_or(0);
            if idx > 0 && idx <= state.positional_params.len() {
                Some(state.positional_params[idx - 1].clone())
            } else {
                None
            }
        }
        _ => state.get_var(name).map(|v| v.to_string()),
    }
}

fn expand_variable(name: &str, state: &mut ShellState) -> String {
    // `${}` and a malformed array subscript are hard errors in bash ("bad
    // substitution", status 1), not silently empty expansions. Rejecting them
    // here also means nothing downstream ever slices a body like `a[`.
    if name.is_empty() || subscript_is_malformed(name) {
        state.expansion_error = Some(format!("${{{}}}: bad substitution", name));
        return String::new();
    }
    match name {
        "?" => state.last_exit_code.to_string(),
        "$" => std::process::id().to_string(),
        "!" => state.last_bg_pid.map_or(String::new(), |p| p.to_string()),
        "#" => state.positional_params.len().to_string(),
        "@" | "*" => state.positional_params.join(" "),
        "0" => state.arg0.clone(),
        "-" => state.option_flags(),
        _ if name.len() <= 3 && name.chars().all(|c| c.is_ascii_digit()) => {
            let idx: usize = name.parse().unwrap_or(0);
            if idx > 0 && idx <= state.positional_params.len() {
                state.positional_params[idx - 1].clone()
            } else {
                String::new()
            }
        }
        _ => expand_parameter(name, state),
    }
}

fn expand_parameter(name: &str, state: &mut ShellState) -> String {
    // ${#arr[@]} / ${#arr[*]} → element count; ${#arr[i]} → length of one
    // element. Bash answers for undeclared names too (0 elements) and counts a
    // plain scalar as one element, so neither form requires a declared array.
    if let Some(inner) = name.strip_prefix('#') {
        if let Some((var_name, subscript)) = split_subscript(inner) {
            if is_name(var_name) {
                if subscript == "@" || subscript == "*" {
                    return element_values(var_name, state).len().to_string();
                }
                return subscript_element(var_name, subscript, state)
                    .unwrap_or_default()
                    .chars()
                    .count()
                    .to_string();
            }
        }
    }

    // ${!var} - Indirect variable reference
    if let Some(var_name) = name.strip_prefix('!') {
        // Check if it's not the special array keys syntax (${!arr[@]})
        if !var_name.contains('[') && !var_name.contains('@') && !var_name.contains('*') {
            // Get the value of the variable named by var_name
            if let Some(ref_name) = state.get_var(var_name) {
                return state.get_var(ref_name).unwrap_or("").to_string();
            }
        }
    }

    // ${!prefix@} and ${!prefix*} - List variable names with prefix
    if let Some(var_spec) = name.strip_prefix('!') {
        if var_spec.ends_with('@') || var_spec.ends_with('*') {
            let prefix = &var_spec[..var_spec.len() - 1];
            let mut names = Vec::new();

            // Collect all variable names starting with prefix
            for k in state.env_vars.keys() {
                if k.starts_with(prefix) {
                    names.push(k.clone());
                }
            }
            // Also collect from all local scopes
            for scope in &state.local_vars_stack {
                for k in scope.keys() {
                    if k.starts_with(prefix) && !names.contains(k) {
                        names.push(k.clone());
                    }
                }
            }

            names.sort();
            if var_spec.ends_with('@') {
                return names
                    .iter()
                    .map(|n| format!("\"{}\"", n))
                    .collect::<Vec<_>>()
                    .join(" ");
            } else {
                return names.join(" ");
            }
        }
    }

    // ${!arr[@]} → array keys
    if let Some(inner) = name.strip_prefix('!') {
        if let Some((var_name, subscript)) = split_subscript(inner) {
            if (subscript == "@" || subscript == "*") && is_name(var_name) {
                if state.is_array(var_name) {
                    return state.array_keys(var_name).join(" ");
                }
                // A scalar stands in for a one-element array, so its only key
                // is 0; an unset name has no keys at all.
                return match state.get_var(var_name) {
                    Some(_) => "0".to_string(),
                    None => String::new(),
                };
            }
        }
    }

    // Array element access and slicing: ${arr[idx]}, ${arr[@]}, ${arr[@]:offset:length}
    if let Some(bracket) = name.find('[') {
        if let Some(bracket_end) = name[bracket..].find(']') {
            let bracket_pos = bracket + bracket_end;
            let var_name = &name[..bracket];
            let subscript_part = &name[bracket + 1..bracket_pos];
            let after_bracket = &name[bracket_pos + 1..];

            // Handle array slicing: ${arr[@]:offset:length} or ${arr[*]:offset:length}
            if subscript_part == "@" || subscript_part == "*" {
                // Check if there's slicing syntax after the bracket
                if let Some(slice_part) = after_bracket.strip_prefix(':') {
                    // Remove the ':'
                    let parts: Vec<&str> = slice_part.split(':').collect();
                    if let Ok(offset) = parts[0].parse::<usize>() {
                        let arr_vals = state.array_values(var_name);
                        let length = if parts.len() > 1 {
                            parts[1].parse::<usize>().unwrap_or(arr_vals.len())
                        } else {
                            arr_vals.len()
                        };
                        let sliced: Vec<String> =
                            arr_vals.iter().skip(offset).take(length).cloned().collect();
                        return sliced.join(" ");
                    }
                } else if state.is_array(var_name) {
                    // No slicing, just return array values
                    return state.array_values(var_name).join(" ");
                }
            }

            // ${arr[@]} or ${arr[*]} as string (without slicing)
            if (subscript_part == "@" || subscript_part == "*") && after_bracket.is_empty() {
                if state.is_array(var_name) {
                    return state.array_values(var_name).join(" ");
                }
                // A scalar stands in for a one-element array.
                if let Some(v) = state.get_var(var_name) {
                    return v.to_string();
                }
            }

            // ${arr[idx]} - single element access
            if after_bracket.is_empty() && state.is_array(var_name) {
                return subscript_element(var_name, subscript_part, state).unwrap_or_default();
            }
            // `x=abc` behaves like `x=(abc)`: `${x[0]}` is the value and every
            // other index is unset. Real scripts index scalars defensively
            // (`${BASH_SOURCE[0]}`, `${PIPESTATUS[0]}`), so answering nothing
            // here silently hands them the wrong value.
            if after_bracket.is_empty() && is_name(var_name) && state.get_var(var_name).is_some() {
                return subscript_element(var_name, subscript_part, state).unwrap_or_default();
            }
        }
    }

    // ${#var} (string length) — the leading `#` is a prefix, not an operator,
    // so this is settled before the operator dispatch below.
    if let Some(var) = name.strip_prefix('#') {
        if !var.is_empty() && !var.contains('#') && !var.contains('[') {
            let val = state.get_var(var).unwrap_or("");
            return val.chars().count().to_string();
        }
    }

    // Everything after the parameter name is one operator plus its word. Bash
    // decides which operator from the character at exactly that boundary, so
    // find the boundary once and dispatch on it. Searching the whole body for
    // each operator in turn let punctuation inside a pattern hijack the
    // expansion: `${v##*a-b*}` was read as `${v##*a}` defaulting to `b*`.
    if let Some(op) = param_op_start(name) {
        let var = &name[..op];
        let spec = &name[op..];

        // ${var:-default} / ${var:=default} / ${var:?message} / ${var:+alt}
        // treat an empty value like an unset one; the colon-less forms below
        // only react to a genuinely unset parameter.
        if let Some(rest) = spec.strip_prefix(":-") {
            return match parameter_value(var, state) {
                Some(v) if !v.is_empty() => v,
                _ => expand_param_operand(rest, state),
            };
        }
        if let Some(rest) = spec.strip_prefix(":=") {
            return match parameter_value(var, state) {
                Some(v) if !v.is_empty() => v,
                _ => {
                    let default = expand_param_operand(rest, state);
                    state.set_var(var, &default);
                    default
                }
            };
        }
        if let Some(rest) = spec.strip_prefix(":+") {
            return match parameter_value(var, state) {
                Some(v) if !v.is_empty() => expand_param_operand(rest, state),
                _ => String::new(),
            };
        }
        if let Some(rest) = spec.strip_prefix(":?") {
            return match parameter_value(var, state) {
                Some(v) if !v.is_empty() => v,
                _ => {
                    let msg = expand_param_operand(rest, state);
                    state.expansion_error = Some(if msg.is_empty() {
                        format!("{}: parameter null or not set", var)
                    } else {
                        format!("{}: {}", var, msg)
                    });
                    String::new()
                }
            };
        }
        if let Some(rest) = spec.strip_prefix('?') {
            return match parameter_value(var, state) {
                Some(v) => v,
                None => {
                    let msg = expand_param_operand(rest, state);
                    state.expansion_error = Some(if msg.is_empty() {
                        format!("{}: parameter not set", var)
                    } else {
                        format!("{}: {}", var, msg)
                    });
                    String::new()
                }
            };
        }
        if let Some(rest) = spec.strip_prefix('-') {
            return parameter_value(var, state)
                .unwrap_or_else(|| expand_param_operand(rest, state));
        }
        if let Some(rest) = spec.strip_prefix('=') {
            return match parameter_value(var, state) {
                Some(v) => v,
                None => {
                    let default = expand_param_operand(rest, state);
                    state.set_var(var, &default);
                    default
                }
            };
        }
        if let Some(rest) = spec.strip_prefix('+') {
            return if parameter_value(var, state).is_some() {
                expand_param_operand(rest, state)
            } else {
                String::new()
            };
        }

        // Pattern operators: #/## (prefix strip), %/%% (suffix strip),
        // / (replace), ^/^^ and ,/,, (case conversion).
        match spec.as_bytes()[0] {
            b'#' => {
                let val = parameter_value(var, state).unwrap_or_default();
                if let Some(pat) = spec.strip_prefix("##") {
                    let pat = expand_param_operand(pat, state);
                    // greedy (longest) prefix strip
                    for i in (0..=val.len()).rev() {
                        if val.is_char_boundary(i) && match_glob(&pat, &val[..i]) {
                            return val[i..].to_string();
                        }
                    }
                } else {
                    let pat = expand_param_operand(&spec[1..], state);
                    for i in 0..=val.len() {
                        if val.is_char_boundary(i) && match_glob(&pat, &val[..i]) {
                            return val[i..].to_string();
                        }
                    }
                }
                return val;
            }
            b'%' => {
                let val = parameter_value(var, state).unwrap_or_default();
                if let Some(pat) = spec.strip_prefix("%%") {
                    let pat = expand_param_operand(pat, state);
                    // greedy (longest) suffix strip
                    for i in 0..=val.len() {
                        if val.is_char_boundary(i) && match_glob(&pat, &val[i..]) {
                            return val[..i].to_string();
                        }
                    }
                } else {
                    let pat = expand_param_operand(&spec[1..], state);
                    for i in (0..=val.len()).rev() {
                        if val.is_char_boundary(i) && match_glob(&pat, &val[i..]) {
                            return val[..i].to_string();
                        }
                    }
                }
                return val;
            }
            b'/' => {
                let val = parameter_value(var, state).unwrap_or_default();
                return pattern_replace(&val, spec, state);
            }
            b'^' | b',' => {
                let val = parameter_value(var, state).unwrap_or_default();
                return convert_case(&val, spec);
            }
            b':' => {
                // ${var:offset} and ${var:offset:length}. The four colon
                // operators above have already been ruled out.
                let val = parameter_value(var, state).unwrap_or_default();
                let rest = &spec[1..];
                let (offset_text, length_text) = match rest.split_once(':') {
                    Some((o, l)) => (o, Some(l)),
                    None => (rest, None),
                };
                // Shell string offsets count characters, not UTF-8 bytes.
                // Keep byte boundaries only for the final slice, and saturate
                // additions so user-supplied integer extremes cannot panic.
                let boundaries: Vec<usize> = val
                    .char_indices()
                    .map(|(i, _)| i)
                    .chain(std::iter::once(val.len()))
                    .collect();
                let count = boundaries.len() - 1;
                // Offset and length are arithmetic expressions: `${s:1+1:2}`.
                let offset = substring_bound(offset_text, state);
                let start = if offset < 0 {
                    (count as i64).saturating_add(offset).max(0) as usize
                } else {
                    (offset as usize).min(count)
                };
                let end = match length_text {
                    Some(l) => {
                        let len = substring_bound(l, state);
                        if len < 0 {
                            (count as i64).saturating_add(len).max(start as i64) as usize
                        } else {
                            start.saturating_add(len as usize).min(count)
                        }
                    }
                    None => count,
                };
                return val[boundaries[start]..boundaries[end]].to_string();
            }
            _ => {}
        }
    }
    if let Some(v) = state.get_var(name) {
        return v.to_string();
    }
    // Phase 5a: fall back to typed let-bindings.
    if let Some(v) = state.let_vars.get(name) {
        return v.to_display_string();
    }
    String::new()
}

/// Evaluate a `${var:offset:length}` bound, which bash treats as arithmetic.
/// A failure leaves an expansion error, and the bound reads as 0.
fn substring_bound(text: &str, state: &mut ShellState) -> i64 {
    match eval_arithmetic(text, state) {
        Ok(value) => value,
        Err(ArithFailure::Expansion) => 0,
        Err(ArithFailure::Eval(message)) => {
            state.expansion_error = Some(message);
            0
        }
    }
}

/// Byte offset where a `${...}` body stops being the parameter name and starts
/// being an operator, or `None` when the body is nothing but a name (or a form
/// the callers above already handled, such as a leading `#` or `!`).
///
/// A name is `[A-Za-z_][A-Za-z0-9_]*` with an optional `[subscript]`, a run of
/// digits for positional parameters, or a single special character. Everything
/// from there on belongs to the operator, punctuation included — which is what
/// keeps `${v##*a-b*}` from being read as `${v-...}`.
fn param_op_start(name: &str) -> Option<usize> {
    let b = name.as_bytes();
    let first = *b.first()?;
    let mut i = 1;
    match first {
        // `#` and `!` lead the length and indirection forms, not a name.
        b'#' | b'!' => return None,
        b'@' | b'*' | b'?' | b'$' | b'-' | b'0' => {}
        b'1'..=b'9' => {
            while i < b.len() && b[i].is_ascii_digit() {
                i += 1;
            }
        }
        b'_' | b'A'..=b'Z' | b'a'..=b'z' => {
            while i < b.len() && (b[i] == b'_' || b[i].is_ascii_alphanumeric()) {
                i += 1;
            }
            if b.get(i) == Some(&b'[') {
                let mut depth = 0usize;
                while i < b.len() {
                    match b[i] {
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
        }
        _ => return None,
    }
    if i < b.len() {
        Some(i)
    } else {
        None
    }
}

/// `${var^}`, `${var^^}`, `${var,}` and `${var,,}`. `spec` starts with the
/// operator character; an optional glob after it limits which characters are
/// converted (bash defaults that pattern to `?`, i.e. every character).
fn convert_case(val: &str, spec: &str) -> String {
    let op = spec.as_bytes()[0];
    let all = spec.len() >= 2 && spec.as_bytes()[1] == op;
    let pat = &spec[if all { 2 } else { 1 }..];
    let matches = |c: char| pat.is_empty() || match_glob(pat, &c.to_string());
    let convert = |c: char| -> String {
        if op == b'^' {
            c.to_uppercase().collect()
        } else {
            c.to_lowercase().collect()
        }
    };
    let mut out = String::with_capacity(val.len());
    for (idx, c) in val.chars().enumerate() {
        // The single-character form only touches the first character.
        if (all || idx == 0) && matches(c) {
            out.push_str(&convert(c));
        } else {
            out.push(c);
        }
    }
    out
}

/// Split a `${var/pat/rep}` body at its first unescaped `/`, returning the raw
/// pattern and replacement with `\/` reduced to a literal slash. A slash that
/// only appears after expanding a variable is data, not the separator, so the
/// split happens before expansion.
fn split_replace_spec(body: &str) -> (String, String) {
    let mut pat = String::new();
    let mut chars = body.chars();
    while let Some(c) = chars.next() {
        match c {
            '\\' => match chars.next() {
                Some('/') => pat.push('/'),
                Some(other) => {
                    pat.push('\\');
                    pat.push(other);
                }
                None => pat.push('\\'),
            },
            '/' => return (pat, chars.as_str().replace("\\/", "/")),
            _ => pat.push(c),
        }
    }
    (pat, String::new())
}

enum ReplaceAnchor {
    None,
    Start,
    End,
}

/// Handle ${var/pat/rep}, ${var//pat/rep}, ${var/#pat/rep}, ${var/%pat/rep}.
/// `spec` begins with '/'. The pattern is a shell glob.
fn pattern_replace(val: &str, spec: &str, state: &mut ShellState) -> String {
    let global = spec.starts_with("//");
    let body = if global { &spec[2..] } else { &spec[1..] };
    let (anchor, body) = if let Some(rest) = body.strip_prefix('#') {
        (ReplaceAnchor::Start, rest)
    } else if let Some(rest) = body.strip_prefix('%') {
        (ReplaceAnchor::End, rest)
    } else {
        (ReplaceAnchor::None, body)
    };
    let (pat, rep) = split_replace_spec(body);
    let pat = expand_param_operand(&pat, state);
    let rep = expand_param_operand(&rep, state);
    let (pat, rep) = (pat.as_str(), rep.as_str());
    if pat.is_empty() {
        return val.to_string();
    }

    match anchor {
        ReplaceAnchor::Start => {
            // longest prefix matching pat
            for i in (1..=val.len()).rev() {
                if val.is_char_boundary(i) && match_glob(pat, &val[..i]) {
                    return format!("{}{}", rep, &val[i..]);
                }
            }
            val.to_string()
        }
        ReplaceAnchor::End => {
            // longest suffix matching pat
            for i in 0..val.len() {
                if val.is_char_boundary(i) && match_glob(pat, &val[i..]) {
                    return format!("{}{}", &val[..i], rep);
                }
            }
            val.to_string()
        }
        ReplaceAnchor::None => {
            let mut result = String::new();
            let mut i = 0;
            let mut done = false;
            while i < val.len() {
                let matched_len = if !done || global {
                    longest_match_at(pat, &val[i..])
                } else {
                    None
                };
                if let Some(l) = matched_len {
                    result.push_str(rep);
                    i += l;
                    done = true;
                } else {
                    let ch = val[i..].chars().next().unwrap();
                    result.push(ch);
                    i += ch.len_utf8();
                }
            }
            result
        }
    }
}

/// Longest non-empty prefix length of `s` matching glob `pat`, if any.
fn longest_match_at(pat: &str, s: &str) -> Option<usize> {
    for l in (1..=s.len()).rev() {
        if s.is_char_boundary(l) && match_glob(pat, &s[..l]) {
            return Some(l);
        }
    }
    None
}

fn expand_brace_items(items: &[Vec<WordPart>], state: &mut ShellState) -> Vec<String> {
    items
        .iter()
        .map(|parts| {
            let mut s = String::new();
            for p in parts {
                s.push_str(&expand_part(p, state));
            }
            s
        })
        .collect()
}

fn expand_brace_range(start: &str, end: &str, step: Option<&str>) -> Vec<String> {
    // Try integer range
    if let (Ok(s), Ok(e)) = (start.parse::<i64>(), end.parse::<i64>()) {
        // `saturating_abs`: `{1..9..-9223372036854775808}` would panic on a
        // plain `abs()`, and a brace range must never be able to kill the shell.
        // Bash reads a zero step as 1.
        let step_abs = step
            .and_then(|s| s.parse::<i64>().ok().map(|v| v.saturating_abs()))
            .unwrap_or(1)
            .max(1);
        let step_val = if s <= e { step_abs } else { -step_abs };

        // An end written with a leading zero (`08`, `-05`) pads every term to
        // the wider end's length, sign included: `{-05..3}` is `-05 ... 003`.
        let pad_width = start.len().max(end.len());
        let zero_padded = |text: &str| {
            let digits = text.strip_prefix('-').unwrap_or(text);
            digits.len() > 1 && digits.starts_with('0')
        };
        let needs_pad = zero_padded(start) || zero_padded(end);

        let mut results = Vec::new();
        let mut i = s;
        loop {
            if step_val > 0 {
                if i > e {
                    break;
                }
            } else if i < e {
                break;
            }
            if needs_pad {
                results.push(format!("{:0width$}", i, width = pad_width));
            } else {
                results.push(i.to_string());
            }
            // `{9223372036854775806..9223372036854775807}` walked off the end of
            // i64 and panicked with "attempt to add with overflow"; the last
            // element is simply the last one there is.
            match i.checked_add(step_val) {
                Some(next) => i = next,
                None => break,
            }
        }
        return results;
    }

    // Try character range
    if start.len() == 1 && end.len() == 1 {
        let s = start.chars().next().unwrap();
        let e = end.chars().next().unwrap();
        // `saturating_abs`/`checked_add`: `{a..z..-2147483648}` panicked on
        // `abs()` and a huge positive step panicked on the `i += step_val` below.
        let step_abs = step
            .and_then(|s| s.parse::<i32>().ok().map(|v| v.saturating_abs()))
            .unwrap_or(1)
            .max(1);
        let step_val = if s <= e { step_abs } else { -step_abs };

        let mut results = Vec::new();
        let mut i = s as i32;
        let end_i = e as i32;
        loop {
            if step_val > 0 {
                if i > end_i {
                    break;
                }
            } else if i < end_i {
                break;
            }
            if let Some(c) = char::from_u32(i as u32) {
                results.push(c.to_string());
            }
            match i.checked_add(step_val) {
                Some(next) => i = next,
                None => break,
            }
        }
        return results;
    }

    vec![]
}

fn match_glob(pattern: &str, text: &str) -> bool {
    crate::glob_match::glob_match(pattern, text)
}

fn expand_tilde(user: &str, state: &mut ShellState) -> String {
    if user.is_empty() {
        state.home_dir.to_string_lossy().to_string()
    } else {
        let c_user = std::ffi::CString::new(user).unwrap_or_default();
        let pw = unsafe { nix::libc::getpwnam(c_user.as_ptr()) };
        if pw.is_null() {
            format!("~{}", user)
        } else {
            let dir = unsafe { std::ffi::CStr::from_ptr((*pw).pw_dir) };
            dir.to_string_lossy().to_string()
        }
    }
}

/// Apply Bash's command-substitution inheritance rules to the forked copy.
///
/// Command substitutions clear `errexit` unless `inherit_errexit` is enabled,
/// and reset an inherited ERR trap unless `errtrace`/`-E` is enabled. This
/// happens before parsing the body, so a trap explicitly installed by the body
/// remains authoritative.
fn prepare_command_substitution_child(state: &mut ShellState) {
    state.interactive = false;
    if !state.shell_opts.inherit_errexit {
        state.shell_opts.errexit = false;
    }
    let inherit_err_trap = state
        .shell_opts
        .tracked_opts
        .get("errtrace")
        .copied()
        .unwrap_or(false);
    if !inherit_err_trap {
        state.traps.remove("ERR");
    }
}

fn expand_command_sub(cmd: &str, state: &mut ShellState) -> String {
    use nix::errno::Errno;
    use nix::sys::wait::WaitStatus;
    use nix::unistd::{close, fork, pipe, read, ForkResult};
    use std::os::unix::io::{BorrowedFd, IntoRawFd};

    let (r, w) = match pipe() {
        Ok(fds) => (fds.0.into_raw_fd(), fds.1.into_raw_fd()),
        Err(_) => {
            state.last_command_sub_status = Some(1);
            return String::new();
        }
    };

    match unsafe { fork() } {
        Ok(ForkResult::Child) => {
            close(r).ok();
            unsafe {
                nix::libc::dup2(w, 1);
            }
            close(w).ok();

            prepare_command_substitution_child(state);
            match crate::parser::parse(cmd) {
                // A command substitution is a complete non-interactive shell
                // program. Reuse its program-level exit/control-flow/errexit
                // gates rather than executing every parsed command
                // unconditionally and letting a later command overwrite the
                // status from `exit` or `set -e`.
                Ok(cmds) => std::process::exit(crate::executor::execute_program(&cmds, state)),
                Err(_) => std::process::exit(2),
            }
        }
        Ok(ForkResult::Parent { child }) => {
            close(w).ok();
            let mut output = Vec::new();
            let mut buf = [0u8; 4096];
            loop {
                // Safe because r is a valid file descriptor
                match unsafe { read(BorrowedFd::borrow_raw(r), &mut buf) } {
                    Ok(0) => break,
                    Ok(n) => output.extend_from_slice(&buf[..n]),
                    // An unrelated signal is not EOF. Preserve cancellation
                    // ownership when the shell has a pending terminating signal.
                    Err(Errno::EINTR) if crate::signal::pending_status().is_none() => continue,
                    Err(_) => break,
                }
            }
            close(r).ok();
            let status = loop {
                match nix::sys::wait::waitpid(child, None) {
                    Ok(WaitStatus::Exited(_, code)) => break code,
                    Ok(WaitStatus::Signaled(_, signal, _)) => break 128 + signal as i32,
                    // No status-changing flags were requested, but keep
                    // waiting if a platform reports an intermediate state.
                    Ok(_) => continue,
                    Err(Errno::EINTR) => continue,
                    Err(_) => break 1,
                }
            };
            // An assignment-only simple command has the status of its last
            // command substitution. Record this after reaping so multiple
            // substitutions naturally leave the rightmost one's status.
            state.last_command_sub_status = Some(status);
            let mut s = String::from_utf8_lossy(&output).to_string();
            while s.ends_with('\n') {
                s.pop();
            }
            s
        }
        Err(_) => {
            close(r).ok();
            close(w).ok();
            state.last_command_sub_status = Some(1);
            String::new()
        }
    }
}

fn expand_process_sub(cmd: &str, kind: &ProcessSubKind, state: &mut ShellState) -> String {
    use nix::unistd::{close, fork, pipe, ForkResult};
    use std::os::unix::io::IntoRawFd;

    let (r, w) = match pipe() {
        Ok(fds) => (fds.0.into_raw_fd(), fds.1.into_raw_fd()),
        Err(_) => return String::new(),
    };

    match unsafe { fork() } {
        Ok(ForkResult::Child) => {
            match kind {
                ProcessSubKind::Input => {
                    // <(cmd): child writes to pipe, parent reads from /dev/fd/N
                    close(r).ok();
                    unsafe {
                        nix::libc::dup2(w, 1);
                    }
                    close(w).ok();
                }
                ProcessSubKind::Output => {
                    // >(cmd): child reads from pipe, parent writes to /dev/fd/N
                    close(w).ok();
                    unsafe {
                        nix::libc::dup2(r, 0);
                    }
                    close(r).ok();
                }
            }
            crate::signal::reset_child_signals();
            // Same inheritance as command substitution: clear errexit unless
            // inherit_errexit, drop ERR trap unless errtrace, then run as a
            // complete non-interactive program (exit/errexit gates).
            prepare_command_substitution_child(state);
            match crate::parser::parse(cmd) {
                Ok(cmds) => std::process::exit(crate::executor::execute_program(&cmds, state)),
                Err(_) => std::process::exit(2),
            }
        }
        Ok(ForkResult::Parent { child }) => {
            state.procsub_pids.push(child.as_raw());
            match kind {
                ProcessSubKind::Input => {
                    close(w).ok();
                    format!("/dev/fd/{}", r)
                }
                ProcessSubKind::Output => {
                    close(r).ok();
                    format!("/dev/fd/{}", w)
                }
            }
        }
        Err(_) => {
            close(r).ok();
            close(w).ok();
            String::new()
        }
    }
}

/// Evaluate `$((...))` to its decimal text. On failure the diagnostic is left
/// in `state.expansion_error`, as for any other expansion error, and `"0"`
/// stands in for the value.
pub fn expand_arithmetic(expr: &str, state: &mut ShellState) -> String {
    match eval_arithmetic(expr, state) {
        Ok(value) => value.to_string(),
        Err(ArithFailure::Expansion) => "0".into(),
        Err(ArithFailure::Eval(message)) => {
            state.expansion_error = Some(message);
            "0".into()
        }
    }
}

/// Why an arithmetic evaluation produced no value.
#[derive(Debug)]
enum ArithFailure {
    /// Expanding the text before evaluation failed (`${x:?}`, a failing
    /// nested `$((...))`). The diagnostic is left in `state.expansion_error`
    /// so the caller reports it like any other expansion error — which bash
    /// treats as fatal even inside `((...))`.
    Expansion,
    /// The expression is malformed or failed while evaluating. The message
    /// names the offending text the way bash does (`1/0: division by 0`).
    Eval(String),
}

/// Expand and evaluate arithmetic text the way `$((...))`, `((...))` and
/// `for ((...))` do: [`expand_arithmetic_text`], then
/// [`evaluate_arithmetic_text`].
fn eval_arithmetic(expr: &str, state: &mut ShellState) -> Result<i64, ArithFailure> {
    let text = expand_arithmetic_text(expr, state).ok_or(ArithFailure::Expansion)?;
    evaluate_arithmetic_text(&text, state).map_err(ArithFailure::Eval)
}

/// Evaluate text that has already been through word expansion, such as the
/// arguments of `let`. Integers are 64-bit and wrap on overflow; `&&`, `||`
/// and `?:` evaluate only the operands they select.
///
/// Bash makes one failure fatal rather than a mere failed expression: an
/// unbound variable under `set -u`. Its diagnostic is also left in
/// `state.expansion_error`, so callers can stop the way they do for any
/// expansion error.
pub fn evaluate_arithmetic_text(text: &str, state: &mut ShellState) -> Result<i64, String> {
    eval_arith_text(text, state).map_err(|error| {
        let message = error.to_string();
        if error.0.fatal {
            state.expansion_error = Some(message.clone());
        }
        message
    })
}

/// Resolve the subscript of `name[subscript]` written outside arithmetic — in
/// `${name[...]}` or `name[...]=value` — to the key it addresses.
///
/// For an associative array the expanded subscript *is* the key. Any other
/// name is (or becomes) an indexed array, whose subscript is an arithmetic
/// expression: `${a[i+1]}`, `a[n-1]=x` and `${a[$((1+1))]}` all work, and the
/// result is the index in decimal. Failures are expansion errors: the
/// diagnostic is left in `state.expansion_error` and `None` is returned.
pub(crate) fn resolve_array_subscript(
    name: &str,
    subscript: &str,
    state: &mut ShellState,
) -> Option<String> {
    if state.is_assoc_array(name) {
        let key = expand_param_operand(subscript, state);
        return state.expansion_error.is_none().then_some(key);
    }
    match eval_arithmetic(subscript, state) {
        Ok(index) => Some(index.to_string()),
        Err(ArithFailure::Expansion) => None,
        Err(ArithFailure::Eval(message)) => {
            state.expansion_error = Some(message);
            None
        }
    }
}

/// How deeply arithmetic evaluation may nest. Parenthesised groups, chains of
/// unary operators, `?:`, assignment chains, `**`, variables whose values are
/// expressions in their own right, and `$((...))` inside the text all count.
/// Bash stops recursive variable evaluation at 1024 levels with "expression
/// recursion level exceeded" (`x=x; $((x))`); the same budget, shared by every
/// form of nesting, keeps hostile input from overflowing the stack.
const MAX_ARITH_DEPTH: usize = 1024;

thread_local! {
    /// Current nesting depth of arithmetic evaluation on this thread. It lives
    /// outside the evaluator because nesting crosses word expansion: an inner
    /// `$((...))` is evaluated by a fresh call from `expand_arithmetic_text`.
    static ARITH_DEPTH: std::cell::Cell<usize> = const { std::cell::Cell::new(0) };
}

/// One level of arithmetic nesting, released when dropped.
struct ArithDepth;

impl ArithDepth {
    fn enter() -> Result<Self, ArithError> {
        ARITH_DEPTH.with(|depth| {
            if depth.get() >= MAX_ARITH_DEPTH {
                return Err(ArithError::from("expression recursion level exceeded"));
            }
            depth.set(depth.get() + 1);
            Ok(ArithDepth)
        })
    }
}

impl Drop for ArithDepth {
    fn drop(&mut self) {
        ARITH_DEPTH.with(|depth| depth.set(depth.get() - 1));
    }
}

/// Bash's expansion of arithmetic text before evaluating it.
///
/// Parameters, command substitutions and nested `$((...))` expand as inside
/// double quotes, and unquoted double quotes are removed (`$(( "1" + 2 ))` is
/// 3). So `$x` splices text (`x='1+2'; $(($x*2))` is 5) while a bare `x` is
/// later evaluated as a sub-expression of its own (`$((x*2))` is 6). `None`
/// means an expansion failed and left its diagnostic in
/// `state.expansion_error`; bash treats that as fatal even inside `((...))`.
pub fn expand_arithmetic_text<'a>(
    expr: &'a str,
    state: &mut ShellState,
) -> Option<std::borrow::Cow<'a, str>> {
    // Nearly all arithmetic is plain text (`i<10`, `n++`); skip the parse.
    if !expr.contains(['$', '`', '"', '\\']) {
        return Some(std::borrow::Cow::Borrowed(expr));
    }
    let _depth = match ArithDepth::enter() {
        Ok(depth) => depth,
        Err(error) => {
            state.expansion_error = Some(error.within(expr).to_string());
            return None;
        }
    };
    let mut text = String::with_capacity(expr.len());
    for part in crate::parser::parse::parse_arithmetic_parts(expr) {
        // Word expansion leaves `set -u` to the executor, which skips
        // arithmetic text; check the references spliced in here.
        if let WordPart::Variable(name) = &part {
            if state.shell_opts.nounset {
                if let Some(param) = crate::executor::unset_parameter(name, state) {
                    state.expansion_error = Some(format!("{param}: unbound variable"));
                    return None;
                }
            }
        }
        let piece = expand_part(&part, state);
        if state.expansion_error.is_some() {
            return None;
        }
        text.push_str(&piece);
    }
    Some(std::borrow::Cow::Owned(text))
}

/// A failed evaluation: the message, and the text bash names in front of it.
/// That is the innermost text being evaluated when evaluation failed — a
/// variable's value that is itself an expression reports against the value
/// (`x=08; $((x))` fails with `08: value too great for base`).
///
/// Boxed so every `Result` handed up the recursive evaluator is one word: in
/// debug builds those slots make up much of the stack each nesting level
/// costs, and nesting is allowed to reach `MAX_ARITH_DEPTH`.
#[derive(Debug)]
struct ArithError(Box<ArithErrorDetail>);

#[derive(Debug)]
struct ArithErrorDetail {
    expression: Option<String>,
    message: String,
    /// Set for an unbound variable under `set -u`, which bash treats as a
    /// fatal expansion error instead of a failed expression.
    fatal: bool,
}

type ArithResult<T = i64> = Result<T, ArithError>;

impl ArithError {
    /// `message`, reported against the text `expression`.
    fn at(expression: &str, message: impl Into<String>) -> Self {
        ArithError(Box::new(ArithErrorDetail {
            expression: Some(expression.trim().to_string()),
            message: message.into(),
            fatal: false,
        }))
    }

    /// `set -u` rejecting a reference to the unset variable `name`.
    fn unbound(name: &str) -> Self {
        let mut error = ArithError::at(name, "unbound variable");
        error.0.fatal = true;
        error
    }

    /// Report against `expression` unless an inner evaluation already named
    /// its own text.
    fn within(mut self, expression: &str) -> Self {
        if self.0.expression.is_none() {
            self.0.expression = Some(expression.trim().to_string());
        }
        self
    }
}

impl From<&str> for ArithError {
    fn from(message: &str) -> Self {
        ArithError::from(message.to_string())
    }
}

impl From<String> for ArithError {
    fn from(message: String) -> Self {
        ArithError(Box::new(ArithErrorDetail {
            expression: None,
            message,
            fatal: false,
        }))
    }
}

impl std::fmt::Display for ArithError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match &self.0.expression {
            Some(expression) => write!(f, "{expression}: {}", self.0.message),
            None => f.write_str(&self.0.message),
        }
    }
}

/// Parse and evaluate expanded arithmetic text.
fn eval_arith_text(text: &str, state: &mut ShellState) -> ArithResult {
    let tokens = tokenize_arith(text);
    // `$(( ))`, `(( ))` and an empty or blank variable all count as 0.
    if tokens.is_empty() {
        return Ok(0);
    }
    let mut parser = ArithParser {
        tokens: &tokens,
        pos: 0,
        state,
    };
    let result = parser.comma(true).and_then(|value| {
        if parser.pos < tokens.len() {
            Err(parser.trailing_error())
        } else {
            Ok(value)
        }
    });
    result.map_err(|error| error.within(text))
}

/// Parse an integer constant: decimal, `0x` hexadecimal, leading-zero octal,
/// or bash's `base#digits` for bases 2 through 64. Digits run 0-9, a-z, A-Z,
/// `@`, `_`; up to base 36 letters are case-insensitive. Values wrap at 64
/// bits like the operators do. This follows bash's `strlong`, error messages
/// included.
fn parse_arith_literal(text: &str) -> Result<i64, String> {
    let bytes = text.as_bytes();
    let mut base = 10u64;
    let mut explicit_base = false;
    let mut i = 0;
    if bytes.first() == Some(&b'0') {
        if bytes.len() == 1 {
            return Ok(0);
        }
        if matches!(bytes[1], b'x' | b'X') {
            base = 16;
            i = 2;
        } else {
            base = 8;
            i = 1;
        }
        explicit_base = true;
    }
    let mut value = 0u64;
    while i < bytes.len() {
        let c = bytes[i];
        i += 1;
        if c == b'#' {
            if explicit_base {
                return Err("invalid number".into());
            }
            if !(2..=64).contains(&value) {
                return Err("invalid arithmetic base".into());
            }
            base = value;
            value = 0;
            explicit_base = true;
            if !bytes.get(i).copied().is_some_and(is_arith_digit) {
                return Err("invalid integer constant".into());
            }
            continue;
        }
        let digit = match c {
            b'0'..=b'9' => c - b'0',
            b'a'..=b'z' => c - b'a' + 10,
            b'A'..=b'Z' if base <= 36 => c - b'A' + 10,
            b'A'..=b'Z' => c - b'A' + 36,
            b'@' => 62,
            b'_' => 63,
            _ => return Err("invalid number".into()),
        };
        if u64::from(digit) >= base {
            return Err("value too great for base".into());
        }
        value = value.wrapping_mul(base).wrapping_add(u64::from(digit));
    }
    Ok(value as i64)
}

/// Characters that may follow `base#` in an integer constant.
fn is_arith_digit(c: u8) -> bool {
    c.is_ascii_alphanumeric() || c == b'@' || c == b'_'
}

/// The integer a variable's text holds when it is a plain constant with an
/// optional sign and surrounding blanks — the common case, decided without
/// building an expression. `None` means the text has to be evaluated as an
/// expression of its own.
fn plain_integer_value(text: &str) -> Option<ArithResult> {
    let trimmed = text.trim_matches([' ', '\t', '\n']);
    let (negative, digits) = match trimmed.as_bytes().first() {
        None => return Some(Ok(0)),
        Some(b'-') => (true, &trimmed[1..]),
        Some(b'+') => (false, &trimmed[1..]),
        Some(_) => (false, trimmed),
    };
    let bytes = digits.as_bytes();
    if !bytes.first().is_some_and(u8::is_ascii_digit)
        || !bytes.iter().all(|&c| is_arith_digit(c) || c == b'#')
    {
        return None;
    }
    Some(
        parse_arith_literal(digits)
            .map(|value| {
                if negative {
                    value.wrapping_neg()
                } else {
                    value
                }
            })
            .map_err(|message| ArithError::at(trimmed, message)),
    )
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum BinaryOp {
    Power,
    Mul,
    Div,
    Rem,
    Add,
    Sub,
    Shl,
    Shr,
    Lt,
    Le,
    Gt,
    Ge,
    Eq,
    Ne,
    BitAnd,
    BitXor,
    BitOr,
    LogicalAnd,
    LogicalOr,
}

impl BinaryOp {
    /// Binding strength from bash's precedence table; higher binds tighter.
    fn precedence(self) -> u8 {
        match self {
            BinaryOp::LogicalOr => 1,
            BinaryOp::LogicalAnd => 2,
            BinaryOp::BitOr => 3,
            BinaryOp::BitXor => 4,
            BinaryOp::BitAnd => 5,
            BinaryOp::Eq | BinaryOp::Ne => 6,
            BinaryOp::Lt | BinaryOp::Le | BinaryOp::Gt | BinaryOp::Ge => 7,
            BinaryOp::Shl | BinaryOp::Shr => 8,
            BinaryOp::Add | BinaryOp::Sub => 9,
            BinaryOp::Mul | BinaryOp::Div | BinaryOp::Rem => 10,
            BinaryOp::Power => 11,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
enum ArithToken {
    Num(i64),
    /// A malformed integer constant or subscript: its text and the problem.
    /// Like bash, the error is raised when the parser reaches it, even inside
    /// a skipped branch.
    BadLiteral {
        text: String,
        message: String,
    },
    /// A character that is no part of arithmetic syntax (`@`, `'`, `$`).
    BadChar,
    /// A variable or array element: the name and the unevaluated subscript.
    Var(String, Option<String>),
    /// A binary operator; `+` and `-` double as the unary operators.
    Binary(BinaryOp),
    /// `=` (`None`) or a compound assignment such as `+=` or `<<=`.
    Assign(Option<BinaryOp>),
    /// `++` (1) or `--` (-1) before an operand.
    Prefix(i64),
    /// `++` (1) or `--` (-1) after an operand.
    Postfix(i64),
    Not,
    BitNot,
    Question,
    Colon,
    Comma,
    LParen,
    RParen,
}

fn tokenize_arith(text: &str) -> Vec<ArithToken> {
    let bytes = text.as_bytes();
    let mut tokens = Vec::new();
    let mut i = 0;
    while i < bytes.len() {
        let c = bytes[i];
        let next = bytes.get(i + 1).copied();
        let (token, width) = match c {
            b' ' | b'\t' | b'\n' => {
                i += 1;
                continue;
            }
            b'\\' if next == Some(b'\n') => {
                i += 2;
                continue;
            }
            b'0'..=b'9' => {
                // Bash reads the longest run of constant characters and only
                // then validates it, so `12abc` is one bad constant.
                let len = bytes[i..]
                    .iter()
                    .take_while(|&&b| is_arith_digit(b) || b == b'#')
                    .count();
                let token = match parse_arith_literal(&text[i..i + len]) {
                    Ok(value) => ArithToken::Num(value),
                    Err(message) => ArithToken::BadLiteral {
                        text: text[i..i + len].to_string(),
                        message,
                    },
                };
                (token, len)
            }
            b'a'..=b'z' | b'A'..=b'Z' | b'_' => {
                let name_len = bytes[i..]
                    .iter()
                    .take_while(|b| b.is_ascii_alphanumeric() || **b == b'_')
                    .count();
                let name = text[i..i + name_len].to_string();
                if bytes.get(i + name_len) != Some(&b'[') {
                    (ArithToken::Var(name, None), name_len)
                } else {
                    let open = i + name_len;
                    match closing_bracket(&bytes[open..]) {
                        Some(close) => (
                            ArithToken::Var(name, Some(text[open + 1..open + close].to_string())),
                            name_len + close + 1,
                        ),
                        None => (
                            ArithToken::BadLiteral {
                                text: text[i..].trim_end().to_string(),
                                message: "bad array subscript".into(),
                            },
                            bytes.len() - i,
                        ),
                    }
                }
            }
            b'+' | b'-' => {
                let (op, delta) = if c == b'+' {
                    (BinaryOp::Add, 1)
                } else {
                    (BinaryOp::Sub, -1)
                };
                if next == Some(c) {
                    // Bash's reading of a doubled sign: after a variable it is
                    // a postfix step; before a name a prefix step; otherwise
                    // two signs, so `--5` is 5 and `1--1` is 2.
                    if matches!(tokens.last(), Some(ArithToken::Var(..))) {
                        (ArithToken::Postfix(delta), 2)
                    } else if starts_with_name(&bytes[i + 2..]) {
                        (ArithToken::Prefix(delta), 2)
                    } else {
                        (ArithToken::Binary(op), 1)
                    }
                } else if next == Some(b'=') {
                    (ArithToken::Assign(Some(op)), 2)
                } else {
                    (ArithToken::Binary(op), 1)
                }
            }
            b'*' => match next {
                Some(b'*') => (ArithToken::Binary(BinaryOp::Power), 2),
                Some(b'=') => (ArithToken::Assign(Some(BinaryOp::Mul)), 2),
                _ => (ArithToken::Binary(BinaryOp::Mul), 1),
            },
            b'/' | b'%' | b'&' | b'^' | b'|' => {
                let op = match c {
                    b'/' => BinaryOp::Div,
                    b'%' => BinaryOp::Rem,
                    b'&' => BinaryOp::BitAnd,
                    b'^' => BinaryOp::BitXor,
                    _ => BinaryOp::BitOr,
                };
                match next {
                    Some(b'&') if c == b'&' => (ArithToken::Binary(BinaryOp::LogicalAnd), 2),
                    Some(b'|') if c == b'|' => (ArithToken::Binary(BinaryOp::LogicalOr), 2),
                    Some(b'=') => (ArithToken::Assign(Some(op)), 2),
                    _ => (ArithToken::Binary(op), 1),
                }
            }
            b'<' | b'>' => {
                let (shift, strict, or_equal) = if c == b'<' {
                    (BinaryOp::Shl, BinaryOp::Lt, BinaryOp::Le)
                } else {
                    (BinaryOp::Shr, BinaryOp::Gt, BinaryOp::Ge)
                };
                match next {
                    Some(n) if n == c => {
                        if bytes.get(i + 2) == Some(&b'=') {
                            (ArithToken::Assign(Some(shift)), 3)
                        } else {
                            (ArithToken::Binary(shift), 2)
                        }
                    }
                    Some(b'=') => (ArithToken::Binary(or_equal), 2),
                    _ => (ArithToken::Binary(strict), 1),
                }
            }
            b'=' => match next {
                Some(b'=') => (ArithToken::Binary(BinaryOp::Eq), 2),
                _ => (ArithToken::Assign(None), 1),
            },
            b'!' => match next {
                Some(b'=') => (ArithToken::Binary(BinaryOp::Ne), 2),
                _ => (ArithToken::Not, 1),
            },
            b'~' => (ArithToken::BitNot, 1),
            b'?' => (ArithToken::Question, 1),
            b':' => (ArithToken::Colon, 1),
            b',' => (ArithToken::Comma, 1),
            b'(' => (ArithToken::LParen, 1),
            b')' => (ArithToken::RParen, 1),
            _ => {
                let width = text[i..].chars().next().map_or(1, char::len_utf8);
                (ArithToken::BadChar, width)
            }
        };
        tokens.push(token);
        i += width;
    }
    tokens
}

/// Offset of the `]` closing the `[` at the start of `bytes`, counting nested
/// brackets (`a[b[1]]`).
fn closing_bracket(bytes: &[u8]) -> Option<usize> {
    let mut depth = 0usize;
    for (offset, &b) in bytes.iter().enumerate() {
        match b {
            b'[' => depth += 1,
            b']' => {
                depth -= 1;
                if depth == 0 {
                    return Some(offset);
                }
            }
            _ => {}
        }
    }
    None
}

/// Does a variable name start here, after optional blanks?
fn starts_with_name(bytes: &[u8]) -> bool {
    bytes
        .iter()
        .find(|b| !matches!(b, b' ' | b'\t' | b'\n'))
        .is_some_and(|b| b.is_ascii_alphabetic() || *b == b'_')
}

/// Apply a binary operator with bash's 64-bit wrapping semantics. `evaluate`
/// is false inside a skipped branch, where division by zero is not an error.
fn apply_binary(op: BinaryOp, left: i64, right: i64, evaluate: bool) -> ArithResult {
    Ok(match op {
        BinaryOp::Power => {
            // Bash rejects a negative exponent even in a skipped branch.
            if right < 0 {
                return Err("exponent less than 0".into());
            }
            wrapping_pow(left, right as u64)
        }
        BinaryOp::Mul => left.wrapping_mul(right),
        BinaryOp::Div | BinaryOp::Rem if right == 0 => {
            if evaluate {
                return Err("division by 0".into());
            }
            0
        }
        BinaryOp::Div => left.wrapping_div(right),
        BinaryOp::Rem => left.wrapping_rem(right),
        BinaryOp::Add => left.wrapping_add(right),
        BinaryOp::Sub => left.wrapping_sub(right),
        // The count is masked to the operand width, as bash does on x86-64.
        BinaryOp::Shl => left.wrapping_shl(right as u32),
        BinaryOp::Shr => left.wrapping_shr(right as u32),
        BinaryOp::Lt => i64::from(left < right),
        BinaryOp::Le => i64::from(left <= right),
        BinaryOp::Gt => i64::from(left > right),
        BinaryOp::Ge => i64::from(left >= right),
        BinaryOp::Eq => i64::from(left == right),
        BinaryOp::Ne => i64::from(left != right),
        BinaryOp::BitAnd => left & right,
        BinaryOp::BitXor => left ^ right,
        BinaryOp::BitOr => left | right,
        BinaryOp::LogicalAnd => i64::from(left != 0 && right != 0),
        BinaryOp::LogicalOr => i64::from(left != 0 || right != 0),
    })
}

/// Exponentiation by squaring with wrapping multiplication; at most 64 rounds
/// whatever the exponent.
fn wrapping_pow(mut base: i64, mut exponent: u64) -> i64 {
    let mut result = 1i64;
    while exponent != 0 {
        if exponent & 1 == 1 {
            result = result.wrapping_mul(base);
        }
        exponent >>= 1;
        base = base.wrapping_mul(base);
    }
    result
}

/// A variable or array element an expression reads or assigns, with its
/// subscript already resolved to the key it addresses.
struct ArithTarget<'t> {
    name: &'t str,
    key: Option<String>,
}

/// What a variable holds, as far as arithmetic is concerned.
enum ArithValue {
    Number(i64),
    /// Text that has to be evaluated as an expression of its own.
    Expression(String),
}

/// Recursive-descent evaluator over arithmetic tokens. Evaluation happens
/// during parsing; `evaluate` is false inside a branch that `&&`, `||` or `?:`
/// skip, so the branch is still parsed (and syntax errors still surface) but
/// reads, assignments and division checks do not happen.
///
/// Every path that can recurse without bound takes an `ArithDepth`, and work
/// that does not recurse lives in helpers off the recursive path, keeping the
/// stack each nesting level costs small.
struct ArithParser<'t, 's> {
    tokens: &'t [ArithToken],
    pos: usize,
    state: &'s mut ShellState,
}

impl<'t> ArithParser<'t, '_> {
    fn eat(&mut self, token: &ArithToken) -> bool {
        if self.tokens.get(self.pos) == Some(token) {
            self.pos += 1;
            true
        } else {
            false
        }
    }

    /// `expr, expr, ...` — the value of the last.
    fn comma(&mut self, evaluate: bool) -> ArithResult {
        let mut value = self.assignment(evaluate)?;
        while self.eat(&ArithToken::Comma) {
            value = self.assignment(evaluate)?;
        }
        Ok(value)
    }

    /// `lvalue = expr` and `lvalue op= expr` (right-associative), or a
    /// conditional expression.
    fn assignment(&mut self, evaluate: bool) -> ArithResult {
        let tokens = self.tokens;
        let (Some(ArithToken::Var(name, subscript)), Some(&ArithToken::Assign(op))) =
            (tokens.get(self.pos), tokens.get(self.pos + 1))
        else {
            return self.ternary(evaluate);
        };
        self.pos += 2;
        let _depth = ArithDepth::enter()?;
        // Bash resolves the target and reads its old value before the right
        // side runs: `x=1; ((x += x++))` leaves 2.
        let target = self.target(name, subscript.as_deref(), evaluate)?;
        let current = match op {
            Some(_) if evaluate => self.read(&target)?,
            _ => 0,
        };
        let right = self.assignment(evaluate)?;
        let value = match op {
            Some(op) => apply_binary(op, current, right, evaluate)?,
            None => right,
        };
        if evaluate {
            self.write(&target, value);
        }
        Ok(value)
    }

    /// `cond ? expr : cond-expr`; only the selected branch is evaluated.
    fn ternary(&mut self, evaluate: bool) -> ArithResult {
        let condition = self.binary(1, evaluate)?;
        if !self.eat(&ArithToken::Question) {
            return Ok(condition);
        }
        let _depth = ArithDepth::enter()?;
        let when_true = self.comma(evaluate && condition != 0)?;
        if !self.eat(&ArithToken::Colon) {
            return Err("`:' expected for conditional expression".into());
        }
        let when_false = self.ternary(evaluate && condition == 0)?;
        Ok(if condition != 0 {
            when_true
        } else {
            when_false
        })
    }

    /// Binary operators by precedence climbing. Every operator is
    /// left-associative except `**`; `&&` and `||` short-circuit.
    fn binary(&mut self, min_precedence: u8, evaluate: bool) -> ArithResult {
        let mut left = self.unary(evaluate)?;
        while let Some(&ArithToken::Binary(op)) = self.tokens.get(self.pos) {
            let precedence = op.precedence();
            if precedence < min_precedence {
                break;
            }
            self.pos += 1;
            let (right_precedence, right_evaluates) = match op {
                BinaryOp::Power => (precedence, evaluate),
                BinaryOp::LogicalAnd => (precedence + 1, evaluate && left != 0),
                BinaryOp::LogicalOr => (precedence + 1, evaluate && left == 0),
                _ => (precedence + 1, evaluate),
            };
            let right = {
                let _depth = ArithDepth::enter()?;
                self.binary(right_precedence, right_evaluates)?
            };
            left = apply_binary(op, left, right, evaluate)?;
        }
        Ok(left)
    }

    /// Prefix operators. They bind tighter than `**`, so `-2**2` is 4.
    fn unary(&mut self, evaluate: bool) -> ArithResult {
        let apply: fn(i64) -> i64 = match self.tokens.get(self.pos) {
            Some(ArithToken::Binary(BinaryOp::Sub)) => i64::wrapping_neg,
            Some(ArithToken::Binary(BinaryOp::Add)) => |value| value,
            Some(ArithToken::Not) => |value| i64::from(value == 0),
            Some(ArithToken::BitNot) => |value| !value,
            Some(&ArithToken::Prefix(delta)) => return self.prefix_step(delta, evaluate),
            _ => return self.operand(evaluate),
        };
        self.pos += 1;
        let _depth = ArithDepth::enter()?;
        Ok(apply(self.unary(evaluate)?))
    }

    /// A constant, a parenthesised expression, or a variable.
    fn operand(&mut self, evaluate: bool) -> ArithResult {
        match self.tokens.get(self.pos) {
            Some(&ArithToken::Num(value)) => {
                self.pos += 1;
                Ok(value)
            }
            Some(ArithToken::LParen) => {
                self.pos += 1;
                let _depth = ArithDepth::enter()?;
                let value = self.comma(evaluate)?;
                if self.eat(&ArithToken::RParen) {
                    Ok(value)
                } else if self.pos < self.tokens.len() {
                    Err(self.trailing_error())
                } else {
                    Err("missing `)'".into())
                }
            }
            Some(ArithToken::Var(..)) => self.variable(evaluate),
            _ => Err(self.operand_error()),
        }
    }

    /// `++name` / `--name`: step the variable, yielding the new value.
    fn prefix_step(&mut self, delta: i64, evaluate: bool) -> ArithResult {
        self.pos += 1;
        let tokens = self.tokens;
        let Some(ArithToken::Var(name, subscript)) = tokens.get(self.pos) else {
            return Err(self.operand_error());
        };
        self.pos += 1;
        let target = self.target(name, subscript.as_deref(), evaluate)?;
        if !evaluate {
            return Ok(0);
        }
        let value = self.read(&target)?.wrapping_add(delta);
        self.write(&target, value);
        Ok(value)
    }

    /// A variable operand, with an optional postfix `++`/`--` that yields the
    /// old value.
    fn variable(&mut self, evaluate: bool) -> ArithResult {
        let tokens = self.tokens;
        let Some(ArithToken::Var(name, subscript)) = tokens.get(self.pos) else {
            return Err(self.operand_error());
        };
        self.pos += 1;
        let target = self.target(name, subscript.as_deref(), evaluate)?;
        let step = match tokens.get(self.pos) {
            Some(&ArithToken::Postfix(delta)) => {
                self.pos += 1;
                Some(delta)
            }
            _ => None,
        };
        if !evaluate {
            return Ok(0);
        }
        let value = self.read(&target)?;
        if let Some(delta) = step {
            self.write(&target, value.wrapping_add(delta));
        }
        Ok(value)
    }

    /// The error for a token found where an operand belongs.
    fn operand_error(&self) -> ArithError {
        match self.tokens.get(self.pos) {
            Some(ArithToken::BadLiteral { text, message }) => ArithError::at(text, message.clone()),
            _ => "syntax error: operand expected".into(),
        }
    }

    /// The error for a token left over where an operator or the end belongs.
    fn trailing_error(&self) -> ArithError {
        match self.tokens.get(self.pos) {
            Some(ArithToken::BadChar) => "syntax error: invalid arithmetic operator".into(),
            _ => "syntax error in expression".into(),
        }
    }

    /// Resolve `name` or `name[subscript]`. An indexed subscript is itself an
    /// arithmetic expression; an associative one is the key text. Inside a
    /// skipped branch the subscript is not evaluated at all, as in bash
    /// (`0 && a[i++]` leaves `i` alone).
    fn target(
        &mut self,
        name: &'t str,
        subscript: Option<&'t str>,
        evaluate: bool,
    ) -> ArithResult<ArithTarget<'t>> {
        let key = match subscript {
            Some(subscript) if evaluate => {
                if subscript.is_empty() || subscript == "@" || subscript == "*" {
                    let reference = format!("{name}[{subscript}]");
                    return Err(ArithError::at(&reference, "bad array subscript"));
                }
                if self.state.is_assoc_array(name) {
                    Some(subscript.to_string())
                } else {
                    let _depth = ArithDepth::enter()?;
                    Some(eval_arith_text(subscript, self.state)?.to_string())
                }
            }
            _ => None,
        };
        Ok(ArithTarget { name, key })
    }

    /// The numeric value of a variable or element. Unset and empty count as 0;
    /// text that is not a plain integer is evaluated as an expression in its
    /// own right, so `a=b; b=3; $((a+1))` is 4 and `a='b=5'; $((a))` assigns.
    fn read(&mut self, target: &ArithTarget<'_>) -> ArithResult {
        match self.lookup(target)? {
            ArithValue::Number(value) => Ok(value),
            ArithValue::Expression(text) => {
                let _depth = ArithDepth::enter().map_err(|error| error.within(target.name))?;
                eval_arith_text(&text, self.state)
            }
        }
    }

    fn lookup(&self, target: &ArithTarget<'_>) -> ArithResult<ArithValue> {
        let state = &*self.state;
        let text = match &target.key {
            Some(key) => state.get_array_element(target.name, key).or_else(|| {
                // A scalar is a one-element array: `x=5; $((x[0]))` is 5.
                (key == "0" && !state.is_array(target.name))
                    .then(|| state.get_var(target.name).map(str::to_string))
                    .flatten()
            }),
            // An array named without a subscript means its element 0.
            None if state.is_array(target.name) => state.get_array_element(target.name, "0"),
            None => match state.get_var(target.name) {
                Some(value) => match plain_integer_value(value) {
                    Some(result) => return result.map(ArithValue::Number),
                    None => Some(value.to_string()),
                },
                None if state.shell_opts.nounset => return Err(ArithError::unbound(target.name)),
                None => None,
            },
        };
        let Some(text) = text else {
            return Ok(ArithValue::Number(0));
        };
        match plain_integer_value(&text) {
            Some(result) => result.map(ArithValue::Number),
            None => Ok(ArithValue::Expression(text)),
        }
    }

    fn write(&mut self, target: &ArithTarget<'_>, value: i64) {
        let value = value.to_string();
        match &target.key {
            Some(key) => self.state.set_array_element(target.name, key, &value),
            None if self.state.is_array(target.name) => {
                self.state.set_array_element(target.name, "0", &value)
            }
            None => self.state.set_var(target.name, &value),
        }
    }
}

/// Does this field contain a glob metacharacter that quoting left eligible?
///
/// This replaced a predicate that re-scanned the *expanded* text for `'`, `"`
/// and `\` to guess which metacharacters had been quoted. That guess is not
/// recoverable — the parser consumed the real quotes long before — so it was
/// wrong in both directions: `echo '*'` globbed, while a filename holding an
/// apostrophe (`$x*` where `x="it's"`) stopped globbing altogether.
fn field_contains_glob(field: &Field) -> bool {
    field.text.char_indices().any(|(byte_pos, c)| {
        matches!(c, '*' | '?' | '[') && field.globbable.get(byte_pos).copied().unwrap_or(false)
    })
}

/// Check if a glob pattern explicitly includes a dot (meaning it will match hidden files).
/// Examples:
/// - "*" -> false (doesn't explicitly match hidden files)
/// - ".*" -> true (explicitly matches hidden files)
/// - "./*" -> true (explicitly includes hidden files)
/// - "*.txt" -> false
/// - ".*.txt" -> true
fn pattern_explicitly_includes_dot(pattern: &str) -> bool {
    let mut escaped = false;
    let mut in_single = false;
    let mut in_double = false;

    for c in pattern.chars() {
        if escaped {
            escaped = false;
            continue;
        }
        match c {
            '\\' => escaped = true,
            '\'' if !in_double => {
                in_single = !in_single;
                continue;
            }
            '"' if !in_single => {
                in_double = !in_double;
                continue;
            }
            _ => {}
        }

        if !in_single && !in_double && c == '.' {
            // Found an explicit dot not in quotes
            return true;
        }
    }
    false
}

/// Handle extglob pattern expansion by directory traversal
fn expand_with_extglob(pattern: &str, state: &mut ShellState) -> Vec<String> {
    use std::fs;

    // Split pattern into directory and file pattern parts
    let (dir_path, file_pattern) = split_pattern_dir(pattern);

    // Get the directory to search
    let search_dir = if dir_path.is_empty() || dir_path == "." {
        std::env::current_dir().unwrap_or_default()
    } else if let Some(home_relative) = dir_path.strip_prefix('~') {
        dirs::home_dir().unwrap_or_default().join(home_relative)
    } else {
        std::path::PathBuf::from(&dir_path)
    };

    // A match keeps the directory part the pattern was written with, `./`
    // included. Reporting the canonical path instead turned `echo !(*.txt)`
    // into a list of absolute paths — and resolved symlinks on the way.
    let literal_prefix = &pattern[..pattern.len() - file_pattern.len()];
    let prefix = if literal_prefix.starts_with('~') {
        format!("{}/", search_dir.to_string_lossy())
    } else {
        literal_prefix.to_string()
    };

    let mut results = Vec::new();

    if let Ok(entries) = fs::read_dir(&search_dir) {
        for entry in entries.flatten() {
            let name = entry.file_name();
            let filename = match name.to_str() {
                Some(f) => f,
                None => continue,
            };
            // Apply dotglob filtering
            if !state.shell_opts.dotglob && filename.starts_with('.') {
                continue;
            }

            // Apply extglob matching
            if crate::glob_match::extglob_match(&file_pattern, filename) {
                results.push(format!("{}{}", prefix, filename));
            }
        }
    }

    if results.is_empty() {
        if state.shell_opts.failglob {
            state.expansion_error = Some(format!("no match: {}", pattern));
            vec![]
        } else if state.shell_opts.nullglob {
            vec![]
        } else {
            vec![pattern.to_string()]
        }
    } else {
        results.sort();
        results
    }
}

/// Split a glob pattern into directory and filename pattern
fn split_pattern_dir(pattern: &str) -> (String, String) {
    // Find the last '/' that is part of the literal path (not in glob syntax)
    let mut last_slash = None;
    let mut in_extglob = false;
    let mut paren_depth = 0;

    let mut previous = None;
    for (i, c) in pattern.char_indices() {
        match c {
            '(' if matches!(previous, Some('!' | '?' | '*' | '+' | '@')) => {
                in_extglob = true;
                paren_depth += 1;
            }
            ')' if in_extglob => {
                paren_depth -= 1;
                if paren_depth == 0 {
                    in_extglob = false;
                }
            }
            '/' if !in_extglob && paren_depth == 0 => {
                last_slash = Some(i);
            }
            _ => {}
        }
        previous = Some(c);
    }

    if let Some(pos) = last_slash {
        let dir = pattern[..pos].to_string();
        let file = pattern[pos + 1..].to_string();
        (if dir.is_empty() { ".".to_string() } else { dir }, file)
    } else {
        (".".to_string(), pattern.to_string())
    }
}

/// Expand all words in a command, performing word splitting on the results.
pub fn expand_words(words: &[Word], state: &mut ShellState) -> Vec<String> {
    state.expansion_error = None;
    let mut result = Vec::new();
    for word in words {
        result.extend(expand_word(word, state));
        if state.expansion_error.is_some() {
            break;
        }
    }
    result
}

/// Expand the words of a `[[ ]]` conditional: one operand per word, always.
///
/// Bash performs neither word splitting nor pathname expansion inside `[[ ]]`,
/// and that is not a nicety. Splitting made `[[ $v == x ]]` read `$v`'s second
/// field as the operator; globbing replaced the pattern in `[[ $f == *.rs ]]`
/// with whatever the directory happened to hold; and dropping the empty field
/// of an unset parameter turned `[[ -n ${ZSH_VERSION-} ]]` into the one-operand
/// test `[[ -n ]]`, which is true — so every bash script that branches on
/// "am I zsh?" took the zsh branch.
///
/// The right operand of `==`, `=`, `!=` and `=~` is a pattern, and in a pattern
/// quoting matters after expansion: `[[ $x == "a*" ]]` compares literally while
/// `[[ $x == a* ]]` globs, and a quoted `=~` operand (or quoted part of one) is
/// a literal string inside the regex. Quote characters are gone by the time the
/// evaluator sees the operands, so the quoted characters are made literal here,
/// where the parts are still known: glob metacharacters become one-character
/// bracket expressions (`*` → `[*]`) and regex text goes through
/// `regex::escape`. The evaluator therefore always treats these operands as
/// patterns.
pub fn expand_conditional_words(words: &[Word], state: &mut ShellState) -> Vec<String> {
    state.expansion_error = None;
    let mut result = Vec::new();
    for (index, word) in words.iter().enumerate() {
        // words[0] is `[[`, so an operator needs index 2 or later to have a
        // left operand.
        let operator = index
            .checked_sub(1)
            .filter(|&op_index| op_index > 1)
            .and_then(|op_index| conditional_pattern_operator(&words[op_index]));
        result.push(match operator {
            Some(kind) => expand_conditional_pattern(word, kind, state),
            None => expand_word_to_string(word, state),
        });
        if state.expansion_error.is_some() {
            break;
        }
    }
    result
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum ConditionalPattern {
    Glob,
    Regex,
}

/// The pattern operators of `[[ ]]`. The parser stores an operator as a single
/// bare literal, so a quoted `"=="` operand is never mistaken for one.
fn conditional_pattern_operator(word: &Word) -> Option<ConditionalPattern> {
    match word.as_slice() {
        [WordPart::Literal(op)] => match op.as_str() {
            "==" | "=" | "!=" => Some(ConditionalPattern::Glob),
            "=~" => Some(ConditionalPattern::Regex),
            _ => None,
        },
        _ => None,
    }
}

/// Expand a `[[ ]]` pattern operand, making every quoted character literal.
fn expand_conditional_pattern(
    word: &Word,
    kind: ConditionalPattern,
    state: &mut ShellState,
) -> String {
    let mut pattern = String::new();
    for part in word {
        let text = expand_part(part, state);
        let active = match kind {
            // Exactly the characters a pathname expansion may glob with.
            ConditionalPattern::Glob => part_is_globbable(part, false),
            // The regex operand's unquoted text arrives as literal parts (the
            // parser protects only glob and brace characters in it), so here
            // only real quoting makes text literal.
            ConditionalPattern::Regex => !matches!(
                part,
                WordPart::SingleQuoted(_) | WordPart::DoubleQuoted(_) | WordPart::Interpolated(_)
            ),
        };
        // Unquoted text is a literal part too (only `*?[` get parts of their
        // own), and its `(`, `|` and `)` are extglob syntax.
        let quoted = !matches!(part, WordPart::Literal(_));
        if active {
            pattern.push_str(&text);
        } else if kind == ConditionalPattern::Regex {
            pattern.push_str(&regex::escape(&text));
        } else {
            for c in text.chars() {
                // A one-character bracket expression is literal in both the
                // plain and the extglob matcher; `\` needs one because the
                // extglob matcher treats it as an escape, and a quoted `(` so
                // it cannot open an extglob group.
                if matches!(c, '*' | '?' | '[' | '\\') || (quoted && c == '(') {
                    pattern.push('[');
                    pattern.push(c);
                    pattern.push(']');
                } else {
                    pattern.push(c);
                }
            }
        }
    }
    pattern
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn extglob_directory_offsets_follow_utf8_boundaries() {
        assert_eq!(
            split_pattern_dir("中文/@(a|b)"),
            ("中文".into(), "@(a|b)".into())
        );
        assert_eq!(
            split_pattern_dir("中文/子目录/@(a|b)"),
            ("中文/子目录".into(), "@(a|b)".into())
        );
        assert_eq!(
            split_pattern_dir("中文/@(a/b|c)"),
            ("中文".into(), "@(a/b|c)".into())
        );
    }

    /// `${a[}` reaches `split_subscript` as the name `a[`, where the old
    /// hand-rolled `&name[bracket + 1..len - 1]` is the reversed range 2..1 and
    /// panics — a typo used to kill the whole shell process.
    #[test]
    fn split_subscript_rejects_unclosed_and_reversed_ranges() {
        assert_eq!(split_subscript("a[0]"), Some(("a", "0")));
        assert_eq!(split_subscript("a[@]"), Some(("a", "@")));
        assert_eq!(split_subscript("a[]"), Some(("a", "")));
        assert_eq!(split_subscript("a[${b[0]}]"), Some(("a", "${b[0]}")));
        // None of these may panic, and none of them is a subscript reference.
        assert_eq!(split_subscript("a["), None);
        assert_eq!(split_subscript("a[}"), None);
        assert_eq!(split_subscript("a[0}"), None);
        assert_eq!(split_subscript("["), None);
        assert_eq!(split_subscript("[]"), Some(("", "")));
        assert_eq!(split_subscript(""), None);
        assert_eq!(split_subscript("plain"), None);
    }

    #[test]
    fn malformed_subscripts_are_bad_substitutions() {
        for body in ["a[", "a[}", "a[0", "a[]", "#a[", "!a[", "arr[0"] {
            assert!(subscript_is_malformed(body), "should be rejected: {body}");
        }
        // Well-formed bodies, and bodies where `[` opens a glob bracket
        // expression inside a pattern operator rather than a subscript.
        for body in [
            "a",
            "a[0]",
            "a[@]",
            "a[*]",
            "#a[@]",
            "!a[@]",
            "v#[a-z]",
            "v%[0-9]",
            "v/[abc]/x",
            "#",
            "!",
            "?",
            "@",
            "*",
            "1",
            "a[@]:1:2",
            "a[i+1]",
        ] {
            assert!(!subscript_is_malformed(body), "should be accepted: {body}");
        }
    }

    #[test]
    fn is_name_matches_plain_parameter_names() {
        assert!(is_name("a"));
        assert!(is_name("_x9"));
        assert!(!is_name(""));
        assert!(!is_name("9a"));
        assert!(!is_name("v#"));
        assert!(!is_name("a-b"));
    }

    /// `{9223372036854775806..9223372036854775807}` used to abort the shell with
    /// "attempt to add with overflow", and an `i64::MIN`/`i32::MIN` step used to
    /// abort it inside `abs()`.
    #[test]
    fn brace_ranges_saturate_instead_of_overflowing() {
        assert_eq!(
            expand_brace_range("9223372036854775806", "9223372036854775807", None),
            vec!["9223372036854775806", "9223372036854775807"]
        );
        assert_eq!(
            expand_brace_range("-9223372036854775807", "-9223372036854775808", None),
            vec!["-9223372036854775807", "-9223372036854775808"]
        );
        assert_eq!(
            expand_brace_range("1", "3", Some("9223372036854775807")),
            vec!["1"]
        );
        assert_eq!(
            expand_brace_range("1", "9", Some("-9223372036854775808")),
            vec!["1"]
        );
        assert_eq!(expand_brace_range("a", "z", Some("-2147483648")), vec!["a"]);
        // The ordinary cases still behave.
        assert_eq!(
            expand_brace_range("1", "5", None),
            vec!["1", "2", "3", "4", "5"]
        );
        assert_eq!(
            expand_brace_range("5", "1", None),
            vec!["5", "4", "3", "2", "1"]
        );
        assert_eq!(
            expand_brace_range("1", "10", Some("3")),
            vec!["1", "4", "7", "10"]
        );
        assert_eq!(
            expand_brace_range("a", "e", None),
            vec!["a", "b", "c", "d", "e"]
        );
        assert_eq!(
            expand_brace_range("e", "a", None),
            vec!["e", "d", "c", "b", "a"]
        );
        // Bash reads a zero step as 1: `{1..3..0}` is `1 2 3`.
        assert_eq!(expand_brace_range("1", "3", Some("0")), vec!["1", "2", "3"]);
        assert_eq!(expand_brace_range("a", "c", Some("0")), vec!["a", "b", "c"]);
    }

    /// Zero-padded ends pad every term to the wider end, sign included.
    #[test]
    fn brace_ranges_pad_like_bash() {
        assert_eq!(
            expand_brace_range("08", "11", None),
            vec!["08", "09", "10", "11"]
        );
        assert_eq!(
            expand_brace_range("-05", "1", None),
            vec!["-05", "-04", "-03", "-02", "-01", "000", "001"]
        );
        assert_eq!(
            expand_brace_range("-1", "02", None),
            vec!["-1", "00", "01", "02"]
        );
        assert_eq!(expand_brace_range("-2", "0", None), vec!["-2", "-1", "0"]);
    }

    #[test]
    fn arithmetic_constants_follow_bash() {
        for (text, value) in [
            ("0", 0),
            ("017", 15),
            ("0x1F", 31),
            ("0X1f", 31),
            ("0x", 0),
            ("2#101", 5),
            ("36#z", 35),
            ("36#Z", 35),
            ("64#z", 35),
            ("64#Z", 61),
            ("64#@", 62),
            ("64#_", 63),
            ("10#010", 10),
            ("9223372036854775808", i64::MIN),
            ("0xffffffffffffffff", -1),
        ] {
            assert_eq!(parse_arith_literal(text), Ok(value), "{text}");
        }
        for (text, message) in [
            ("08", "value too great for base"),
            ("1e3", "value too great for base"),
            ("2#102", "value too great for base"),
            ("0x1G", "value too great for base"),
            ("65#1", "invalid arithmetic base"),
            ("1#1", "invalid arithmetic base"),
            ("16#", "invalid integer constant"),
            ("2##1", "invalid integer constant"),
            ("0#5", "invalid number"),
            ("2#1#1", "invalid number"),
        ] {
            assert_eq!(parse_arith_literal(text), Err(message.into()), "{text}");
        }
    }

    /// A doubled sign is a postfix step after a variable, a prefix step before
    /// a name, and otherwise two signs: bash reads `--5` as 5 and `1--1` as 2.
    #[test]
    fn doubled_signs_tokenize_like_bash() {
        use ArithToken::{Assign, Binary, Num, Postfix, Prefix, Var};
        let var = |name: &str| Var(name.into(), None);
        assert_eq!(tokenize_arith("x++"), [var("x"), Postfix(1)]);
        assert_eq!(tokenize_arith("x ++"), [var("x"), Postfix(1)]);
        assert_eq!(tokenize_arith("-- x"), [Prefix(-1), var("x")]);
        assert_eq!(
            tokenize_arith("--5"),
            [Binary(BinaryOp::Sub), Binary(BinaryOp::Sub), Num(5)]
        );
        assert_eq!(
            tokenize_arith("1--1"),
            [Num(1), Binary(BinaryOp::Sub), Binary(BinaryOp::Sub), Num(1)]
        );
        assert_eq!(
            tokenize_arith("a+++b"),
            [var("a"), Postfix(1), Binary(BinaryOp::Add), var("b")]
        );
        assert_eq!(
            tokenize_arith("a[i+1]--"),
            [Var("a".into(), Some("i+1".into())), Postfix(-1)]
        );
        assert_eq!(
            tokenize_arith("x<<=2**3"),
            [
                var("x"),
                Assign(Some(BinaryOp::Shl)),
                Num(2),
                Binary(BinaryOp::Power),
                Num(3)
            ]
        );
    }

    #[test]
    fn malformed_tokens_are_kept_for_the_parser_to_report() {
        assert_eq!(
            tokenize_arith("a[1 + 2"),
            [ArithToken::BadLiteral {
                text: "a[1 + 2".into(),
                message: "bad array subscript".into(),
            }]
        );
        assert_eq!(
            tokenize_arith("1 @ é"),
            [ArithToken::Num(1), ArithToken::BadChar, ArithToken::BadChar]
        );
    }

    #[test]
    fn plain_integer_values_skip_expression_evaluation() {
        let value = |text: &str| plain_integer_value(text).map(|result| result.ok());
        assert_eq!(value(""), Some(Some(0)));
        assert_eq!(value(" 42 "), Some(Some(42)));
        assert_eq!(value("-010"), Some(Some(-8)));
        assert_eq!(value("+0x10"), Some(Some(16)));
        assert_eq!(value("08"), Some(None));
        assert_eq!(value("1+2"), None);
        assert_eq!(value("b"), None);
        assert_eq!(value("- 3"), None);
    }

    #[test]
    fn wrapping_pow_wraps_like_repeated_multiplication() {
        assert_eq!(wrapping_pow(2, 10), 1024);
        assert_eq!(wrapping_pow(2, 63), i64::MIN);
        assert_eq!(wrapping_pow(2, 64), 0);
        assert_eq!(wrapping_pow(3, 40), 3i64.wrapping_pow(40));
        assert_eq!(wrapping_pow(-1, u64::MAX), -1);
        assert_eq!(wrapping_pow(0, 0), 1);
    }
}
