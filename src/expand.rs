/// Variable, tilde, glob, command substitution, arithmetic, array, and process
/// substitution expansion.
use crate::environment::ShellState;
use crate::parser::ast::{InterpPart, PathSeg, ProcessSubKind, Word, WordPart};
use crate::value::Value;

/// Expand a Word (Vec<WordPart>) into a list of strings.
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
/// array so that `${x[0]}` on `x=abc` yields `abc` the way bash does.
fn subscript_element(var_name: &str, subscript: &str, state: &ShellState) -> Option<String> {
    if state.is_array(var_name) {
        return state.get_array_element(var_name, subscript);
    }
    let value = state.get_var(var_name)?;
    match subscript.trim().parse::<usize>() {
        Ok(0) => Some(value.to_string()),
        _ => None,
    }
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
                    .len()
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
            for (k, _) in state.env_vars.iter() {
                if k.starts_with(prefix) {
                    names.push(k.clone());
                }
            }
            // Also collect from all local scopes
            for scope in &state.local_vars_stack {
                for (k, _) in scope.iter() {
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
                if after_bracket.starts_with(':') {
                    let slice_part = &after_bracket[1..]; // Remove the ':'
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
                return state
                    .get_array_element(var_name, subscript_part)
                    .unwrap_or_default();
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
            return val.len().to_string();
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
            let default = expand_param_operand(rest, state);
            return match parameter_value(var, state) {
                Some(v) if !v.is_empty() => v,
                _ => default,
            };
        }
        if let Some(rest) = spec.strip_prefix(":=") {
            let default = expand_param_operand(rest, state);
            return match parameter_value(var, state) {
                Some(v) if !v.is_empty() => v,
                _ => {
                    state.set_var(var, &default);
                    default
                }
            };
        }
        if let Some(rest) = spec.strip_prefix(":+") {
            let alt = expand_param_operand(rest, state);
            return match parameter_value(var, state) {
                Some(v) if !v.is_empty() => alt,
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
            let default = expand_param_operand(rest, state);
            return parameter_value(var, state).unwrap_or(default);
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
            let alt = expand_param_operand(rest, state);
            return if parameter_value(var, state).is_some() {
                alt
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
                let offset: i64 = offset_text.trim().parse().unwrap_or(0);
                let start = if offset < 0 {
                    (val.len() as i64 + offset).max(0) as usize
                } else {
                    (offset as usize).min(val.len())
                };
                let end = match length_text {
                    Some(l) => {
                        let len: i64 = l.trim().parse().unwrap_or(val.len() as i64);
                        if len < 0 {
                            // A negative length is an offset from the end.
                            ((val.len() as i64 + len).max(start as i64) as usize).min(val.len())
                        } else {
                            (start + len as usize).min(val.len())
                        }
                    }
                    None => val.len(),
                };
                return val.get(start..end).unwrap_or("").to_string();
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
        let step_abs = step
            .and_then(|s| s.parse::<i64>().ok().map(|v| v.saturating_abs()))
            .unwrap_or(1);
        if step_abs == 0 {
            return vec![];
        }
        let step_val = if s <= e { step_abs } else { -step_abs };

        // Check for zero-padding
        let pad_width = start.len().max(end.len());
        let needs_pad =
            (start.starts_with('0') && start.len() > 1) || (end.starts_with('0') && end.len() > 1);

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
                results.push(format!("{:0>width$}", i, width = pad_width));
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
            .unwrap_or(1);
        if step_abs == 0 {
            return vec![];
        }
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

fn expand_command_sub(cmd: &str, state: &mut crate::environment::ShellState) -> String {
    use nix::unistd::{close, fork, pipe, read, ForkResult};
    use std::os::unix::io::{BorrowedFd, IntoRawFd};

    let (r, w) = match pipe() {
        Ok(fds) => (fds.0.into_raw_fd(), fds.1.into_raw_fd()),
        Err(_) => return String::new(),
    };

    match unsafe { fork() } {
        Ok(ForkResult::Child) => {
            close(r).ok();
            unsafe {
                nix::libc::dup2(w, 1);
            }
            close(w).ok();

            state.interactive = false;
            match crate::parser::parse(cmd) {
                Ok(cmds) => {
                    let mut code = 0;
                    for c in &cmds {
                        code = crate::executor::execute_complete_command(c, state);
                    }
                    std::process::exit(code);
                }
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
                    Ok(0) | Err(_) => break,
                    Ok(n) => output.extend_from_slice(&buf[..n]),
                }
            }
            close(r).ok();
            nix::sys::wait::waitpid(child, None).ok();
            let mut s = String::from_utf8_lossy(&output).to_string();
            while s.ends_with('\n') || s.ends_with('\r') {
                s.pop();
            }
            s
        }
        Err(_) => {
            close(r).ok();
            close(w).ok();
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
            state.interactive = false;
            match crate::parser::parse(cmd) {
                Ok(cmds) => {
                    let mut code = 0;
                    for c in &cmds {
                        code = crate::executor::execute_complete_command(c, state);
                    }
                    std::process::exit(code);
                }
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

pub fn expand_arithmetic(expr: &str, state: &mut ShellState) -> String {
    let tokens = tokenize_arith(expr);
    match eval_arith_expr(&tokens, &mut 0, state) {
        Ok(n) => n.to_string(),
        Err(_) => String::from("0"),
    }
}

fn tokenize_arith(expr: &str) -> Vec<ArithToken> {
    let mut tokens = Vec::new();
    let mut chars = expr.chars().peekable();
    while let Some(&c) = chars.peek() {
        match c {
            ' ' | '\t' | '\n' => {
                chars.next();
            }
            '0'..='9' => {
                let mut n = String::new();
                // Handle hex (0x), octal (0) prefixes
                if c == '0' {
                    n.push(c);
                    chars.next();
                    match chars.peek() {
                        Some(&'x') | Some(&'X') => {
                            n.push('x');
                            chars.next();
                            while let Some(&d) = chars.peek() {
                                if d.is_ascii_hexdigit() {
                                    n.push(d);
                                    chars.next();
                                } else {
                                    break;
                                }
                            }
                        }
                        _ => {
                            while let Some(&d) = chars.peek() {
                                if d.is_ascii_digit() {
                                    n.push(d);
                                    chars.next();
                                } else {
                                    break;
                                }
                            }
                        }
                    }
                } else {
                    while let Some(&d) = chars.peek() {
                        if d.is_ascii_digit() {
                            n.push(d);
                            chars.next();
                        } else {
                            break;
                        }
                    }
                }
                let val = if n.starts_with("0x") || n.starts_with("0X") {
                    i64::from_str_radix(&n[2..], 16).unwrap_or(0)
                } else if n.starts_with('0') && n.len() > 1 {
                    i64::from_str_radix(&n[1..], 8).unwrap_or(0)
                } else {
                    n.parse().unwrap_or(0)
                };
                tokens.push(ArithToken::Num(val));
            }
            'a'..='z' | 'A'..='Z' | '_' => {
                let mut name = String::new();
                while let Some(&c2) = chars.peek() {
                    if c2.is_alphanumeric() || c2 == '_' {
                        name.push(c2);
                        chars.next();
                    } else {
                        break;
                    }
                }
                tokens.push(ArithToken::Ident(name));
            }
            '$' => {
                chars.next();
                let mut name = String::new();
                while let Some(&c2) = chars.peek() {
                    if c2.is_alphanumeric() || c2 == '_' {
                        name.push(c2);
                        chars.next();
                    } else {
                        break;
                    }
                }
                tokens.push(ArithToken::Ident(name));
            }
            '+' => {
                chars.next();
                match chars.peek() {
                    Some(&'+') => {
                        chars.next();
                        tokens.push(ArithToken::Increment);
                    }
                    Some(&'=') => {
                        chars.next();
                        tokens.push(ArithToken::PlusAssign);
                    }
                    _ => tokens.push(ArithToken::Plus),
                }
            }
            '-' => {
                chars.next();
                match chars.peek() {
                    Some(&'-') => {
                        chars.next();
                        tokens.push(ArithToken::Decrement);
                    }
                    Some(&'=') => {
                        chars.next();
                        tokens.push(ArithToken::MinusAssign);
                    }
                    _ => tokens.push(ArithToken::Minus),
                }
            }
            '*' => {
                chars.next();
                if chars.peek() == Some(&'=') {
                    chars.next();
                    tokens.push(ArithToken::StarAssign);
                } else {
                    tokens.push(ArithToken::Star);
                }
            }
            '/' => {
                chars.next();
                if chars.peek() == Some(&'=') {
                    chars.next();
                    tokens.push(ArithToken::SlashAssign);
                } else {
                    tokens.push(ArithToken::Slash);
                }
            }
            '%' => {
                chars.next();
                if chars.peek() == Some(&'=') {
                    chars.next();
                    tokens.push(ArithToken::PercentAssign);
                } else {
                    tokens.push(ArithToken::Percent);
                }
            }
            '(' => {
                chars.next();
                tokens.push(ArithToken::LParen);
            }
            ')' => {
                chars.next();
                tokens.push(ArithToken::RParen);
            }
            '?' => {
                chars.next();
                tokens.push(ArithToken::Question);
            }
            ':' => {
                chars.next();
                tokens.push(ArithToken::Colon);
            }
            '~' => {
                chars.next();
                tokens.push(ArithToken::BitNot);
            }
            ',' => {
                chars.next();
                tokens.push(ArithToken::Comma);
            }
            '&' => {
                chars.next();
                if chars.peek() == Some(&'&') {
                    chars.next();
                    tokens.push(ArithToken::LogicalAnd);
                } else {
                    tokens.push(ArithToken::BitAnd);
                }
            }
            '|' => {
                chars.next();
                if chars.peek() == Some(&'|') {
                    chars.next();
                    tokens.push(ArithToken::LogicalOr);
                } else {
                    tokens.push(ArithToken::BitOr);
                }
            }
            '^' => {
                chars.next();
                tokens.push(ArithToken::BitXor);
            }
            '<' => {
                chars.next();
                match chars.peek() {
                    Some(&'=') => {
                        chars.next();
                        tokens.push(ArithToken::Le);
                    }
                    Some(&'<') => {
                        chars.next();
                        tokens.push(ArithToken::LShift);
                    }
                    _ => tokens.push(ArithToken::Lt),
                }
            }
            '>' => {
                chars.next();
                match chars.peek() {
                    Some(&'=') => {
                        chars.next();
                        tokens.push(ArithToken::Ge);
                    }
                    Some(&'>') => {
                        chars.next();
                        tokens.push(ArithToken::RShift);
                    }
                    _ => tokens.push(ArithToken::Gt),
                }
            }
            '=' => {
                chars.next();
                if chars.peek() == Some(&'=') {
                    chars.next();
                    tokens.push(ArithToken::Eq);
                } else {
                    tokens.push(ArithToken::Assign);
                }
            }
            '!' => {
                chars.next();
                if chars.peek() == Some(&'=') {
                    chars.next();
                    tokens.push(ArithToken::Ne);
                } else {
                    tokens.push(ArithToken::Not);
                }
            }
            _ => {
                chars.next();
            }
        }
    }
    tokens
}

#[derive(Debug, Clone)]
enum ArithToken {
    Num(i64),
    Ident(String),
    Plus,
    Minus,
    Star,
    Slash,
    Percent,
    Increment,
    Decrement,
    Assign,
    PlusAssign,
    MinusAssign,
    StarAssign,
    SlashAssign,
    PercentAssign,
    BitAnd,
    BitOr,
    BitXor,
    BitNot,
    LShift,
    RShift,
    LogicalAnd,
    LogicalOr,
    Not,
    Lt,
    Le,
    Gt,
    Ge,
    Eq,
    Ne,
    Question,
    Colon,
    Comma,
    LParen,
    RParen,
}

fn eval_arith_expr(
    tokens: &[ArithToken],
    pos: &mut usize,
    state: &mut ShellState,
) -> Result<i64, String> {
    let val = eval_arith_assign(tokens, pos, state)?;
    if *pos < tokens.len() && matches!(tokens.get(*pos), Some(ArithToken::Comma)) {
        *pos += 1;
        return eval_arith_expr(tokens, pos, state);
    }
    Ok(val)
}

fn eval_arith_assign(
    tokens: &[ArithToken],
    pos: &mut usize,
    state: &mut ShellState,
) -> Result<i64, String> {
    let save_pos = *pos;
    if let Some(ArithToken::Ident(name)) = tokens.get(*pos) {
        let name = name.clone();
        *pos += 1;
        match tokens.get(*pos) {
            Some(ArithToken::Assign) => {
                *pos += 1;
                let val = eval_arith_assign(tokens, pos, state)?;
                state.set_var(&name, &val.to_string());
                return Ok(val);
            }
            Some(ArithToken::PlusAssign) => {
                *pos += 1;
                let cur = get_var_value(&name, state);
                let val = cur + eval_arith_assign(tokens, pos, state)?;
                state.set_var(&name, &val.to_string());
                return Ok(val);
            }
            Some(ArithToken::MinusAssign) => {
                *pos += 1;
                let cur = get_var_value(&name, state);
                let val = cur - eval_arith_assign(tokens, pos, state)?;
                state.set_var(&name, &val.to_string());
                return Ok(val);
            }
            Some(ArithToken::StarAssign) => {
                *pos += 1;
                let cur = get_var_value(&name, state);
                let val = cur * eval_arith_assign(tokens, pos, state)?;
                state.set_var(&name, &val.to_string());
                return Ok(val);
            }
            Some(ArithToken::SlashAssign) => {
                *pos += 1;
                let cur = get_var_value(&name, state);
                let rhs = eval_arith_assign(tokens, pos, state)?;
                if rhs == 0 {
                    return Err("division by zero".into());
                }
                let val = cur / rhs;
                state.set_var(&name, &val.to_string());
                return Ok(val);
            }
            Some(ArithToken::PercentAssign) => {
                *pos += 1;
                let cur = get_var_value(&name, state);
                let rhs = eval_arith_assign(tokens, pos, state)?;
                if rhs == 0 {
                    return Err("division by zero".into());
                }
                let val = cur % rhs;
                state.set_var(&name, &val.to_string());
                return Ok(val);
            }
            _ => {}
        }
    }
    *pos = save_pos;
    eval_arith_ternary(tokens, pos, state)
}

fn eval_arith_ternary(
    tokens: &[ArithToken],
    pos: &mut usize,
    state: &mut ShellState,
) -> Result<i64, String> {
    let cond = eval_arith_logical_or(tokens, pos, state)?;
    if *pos < tokens.len() && matches!(tokens.get(*pos), Some(ArithToken::Question)) {
        *pos += 1;
        let true_val = eval_arith_assign(tokens, pos, state)?;
        if *pos < tokens.len() && matches!(tokens.get(*pos), Some(ArithToken::Colon)) {
            *pos += 1;
            let false_val = eval_arith_assign(tokens, pos, state)?;
            return Ok(if cond != 0 { true_val } else { false_val });
        }
    }
    Ok(cond)
}

fn eval_arith_logical_or(
    tokens: &[ArithToken],
    pos: &mut usize,
    state: &mut ShellState,
) -> Result<i64, String> {
    let mut left = eval_arith_logical_and(tokens, pos, state)?;
    while *pos < tokens.len() && matches!(tokens.get(*pos), Some(ArithToken::LogicalOr)) {
        *pos += 1;
        if left != 0 {
            let _ = eval_arith_logical_and(tokens, pos, state)?;
            return Ok(1);
        }
        let right = eval_arith_logical_and(tokens, pos, state)?;
        left = if right != 0 { 1 } else { 0 };
    }
    Ok(left)
}

fn eval_arith_logical_and(
    tokens: &[ArithToken],
    pos: &mut usize,
    state: &mut ShellState,
) -> Result<i64, String> {
    let mut left = eval_arith_bitwise_or(tokens, pos, state)?;
    while *pos < tokens.len() && matches!(tokens.get(*pos), Some(ArithToken::LogicalAnd)) {
        *pos += 1;
        if left == 0 {
            let _ = eval_arith_bitwise_or(tokens, pos, state)?;
            return Ok(0);
        }
        let right = eval_arith_bitwise_or(tokens, pos, state)?;
        left = if right != 0 { 1 } else { 0 };
    }
    Ok(left)
}

fn eval_arith_bitwise_or(
    tokens: &[ArithToken],
    pos: &mut usize,
    state: &mut ShellState,
) -> Result<i64, String> {
    let mut left = eval_arith_bitwise_xor(tokens, pos, state)?;
    while *pos < tokens.len() && matches!(tokens.get(*pos), Some(ArithToken::BitOr)) {
        *pos += 1;
        left |= eval_arith_bitwise_xor(tokens, pos, state)?;
    }
    Ok(left)
}

fn eval_arith_bitwise_xor(
    tokens: &[ArithToken],
    pos: &mut usize,
    state: &mut ShellState,
) -> Result<i64, String> {
    let mut left = eval_arith_bitwise_and(tokens, pos, state)?;
    while *pos < tokens.len() && matches!(tokens.get(*pos), Some(ArithToken::BitXor)) {
        *pos += 1;
        left ^= eval_arith_bitwise_and(tokens, pos, state)?;
    }
    Ok(left)
}

fn eval_arith_bitwise_and(
    tokens: &[ArithToken],
    pos: &mut usize,
    state: &mut ShellState,
) -> Result<i64, String> {
    let mut left = eval_arith_comparison(tokens, pos, state)?;
    while *pos < tokens.len() && matches!(tokens.get(*pos), Some(ArithToken::BitAnd)) {
        *pos += 1;
        left &= eval_arith_comparison(tokens, pos, state)?;
    }
    Ok(left)
}

fn eval_arith_comparison(
    tokens: &[ArithToken],
    pos: &mut usize,
    state: &mut ShellState,
) -> Result<i64, String> {
    let mut left = eval_arith_shift(tokens, pos, state)?;
    while *pos < tokens.len() {
        match tokens.get(*pos) {
            Some(ArithToken::Lt) => {
                *pos += 1;
                let r = eval_arith_shift(tokens, pos, state)?;
                left = if left < r { 1 } else { 0 };
            }
            Some(ArithToken::Le) => {
                *pos += 1;
                let r = eval_arith_shift(tokens, pos, state)?;
                left = if left <= r { 1 } else { 0 };
            }
            Some(ArithToken::Gt) => {
                *pos += 1;
                let r = eval_arith_shift(tokens, pos, state)?;
                left = if left > r { 1 } else { 0 };
            }
            Some(ArithToken::Ge) => {
                *pos += 1;
                let r = eval_arith_shift(tokens, pos, state)?;
                left = if left >= r { 1 } else { 0 };
            }
            Some(ArithToken::Eq) => {
                *pos += 1;
                let r = eval_arith_shift(tokens, pos, state)?;
                left = if left == r { 1 } else { 0 };
            }
            Some(ArithToken::Ne) => {
                *pos += 1;
                let r = eval_arith_shift(tokens, pos, state)?;
                left = if left != r { 1 } else { 0 };
            }
            _ => break,
        }
    }
    Ok(left)
}

fn eval_arith_shift(
    tokens: &[ArithToken],
    pos: &mut usize,
    state: &mut ShellState,
) -> Result<i64, String> {
    let mut left = eval_arith_additive(tokens, pos, state)?;
    while *pos < tokens.len() {
        match tokens.get(*pos) {
            Some(ArithToken::LShift) => {
                *pos += 1;
                left <<= eval_arith_additive(tokens, pos, state)?;
            }
            Some(ArithToken::RShift) => {
                *pos += 1;
                left >>= eval_arith_additive(tokens, pos, state)?;
            }
            _ => break,
        }
    }
    Ok(left)
}

fn eval_arith_additive(
    tokens: &[ArithToken],
    pos: &mut usize,
    state: &mut ShellState,
) -> Result<i64, String> {
    let mut left = eval_arith_term(tokens, pos, state)?;
    while *pos < tokens.len() {
        match tokens.get(*pos) {
            Some(ArithToken::Plus) => {
                *pos += 1;
                left += eval_arith_term(tokens, pos, state)?;
            }
            Some(ArithToken::Minus) => {
                *pos += 1;
                left -= eval_arith_term(tokens, pos, state)?;
            }
            _ => break,
        }
    }
    Ok(left)
}

fn eval_arith_term(
    tokens: &[ArithToken],
    pos: &mut usize,
    state: &mut ShellState,
) -> Result<i64, String> {
    let mut left = eval_arith_unary(tokens, pos, state)?;
    while *pos < tokens.len() {
        match tokens.get(*pos) {
            Some(ArithToken::Star) => {
                *pos += 1;
                left *= eval_arith_unary(tokens, pos, state)?;
            }
            Some(ArithToken::Slash) => {
                *pos += 1;
                let r = eval_arith_unary(tokens, pos, state)?;
                if r == 0 {
                    return Err("division by zero".into());
                }
                left /= r;
            }
            Some(ArithToken::Percent) => {
                *pos += 1;
                let r = eval_arith_unary(tokens, pos, state)?;
                if r == 0 {
                    return Err("division by zero".into());
                }
                left %= r;
            }
            _ => break,
        }
    }
    Ok(left)
}

fn eval_arith_unary(
    tokens: &[ArithToken],
    pos: &mut usize,
    state: &mut ShellState,
) -> Result<i64, String> {
    match tokens.get(*pos) {
        Some(ArithToken::Minus) => {
            *pos += 1;
            Ok(-eval_arith_unary(tokens, pos, state)?)
        }
        Some(ArithToken::Plus) => {
            *pos += 1;
            eval_arith_unary(tokens, pos, state)
        }
        Some(ArithToken::Not) => {
            *pos += 1;
            let v = eval_arith_unary(tokens, pos, state)?;
            Ok(if v == 0 { 1 } else { 0 })
        }
        Some(ArithToken::BitNot) => {
            *pos += 1;
            Ok(!eval_arith_unary(tokens, pos, state)?)
        }
        Some(ArithToken::Increment) => {
            *pos += 1;
            if let Some(ArithToken::Ident(name)) = tokens.get(*pos) {
                let name = name.clone();
                *pos += 1;
                let val = get_var_value(&name, state) + 1;
                state.set_var(&name, &val.to_string());
                Ok(val)
            } else {
                Ok(0)
            }
        }
        Some(ArithToken::Decrement) => {
            *pos += 1;
            if let Some(ArithToken::Ident(name)) = tokens.get(*pos) {
                let name = name.clone();
                *pos += 1;
                let val = get_var_value(&name, state) - 1;
                state.set_var(&name, &val.to_string());
                Ok(val)
            } else {
                Ok(0)
            }
        }
        _ => eval_arith_postfix(tokens, pos, state),
    }
}

fn eval_arith_postfix(
    tokens: &[ArithToken],
    pos: &mut usize,
    state: &mut ShellState,
) -> Result<i64, String> {
    if let Some(ArithToken::Ident(name)) = tokens.get(*pos) {
        let name = name.clone();
        *pos += 1;
        match tokens.get(*pos) {
            Some(ArithToken::Increment) => {
                *pos += 1;
                let val = get_var_value(&name, state);
                state.set_var(&name, &(val + 1).to_string());
                return Ok(val);
            }
            Some(ArithToken::Decrement) => {
                *pos += 1;
                let val = get_var_value(&name, state);
                state.set_var(&name, &(val - 1).to_string());
                return Ok(val);
            }
            _ => return Ok(get_var_value(&name, state)),
        }
    }
    eval_arith_primary(tokens, pos, state)
}

fn eval_arith_primary(
    tokens: &[ArithToken],
    pos: &mut usize,
    state: &mut ShellState,
) -> Result<i64, String> {
    match tokens.get(*pos) {
        Some(ArithToken::Num(n)) => {
            let n = *n;
            *pos += 1;
            Ok(n)
        }
        Some(ArithToken::LParen) => {
            *pos += 1;
            let v = eval_arith_expr(tokens, pos, state)?;
            if matches!(tokens.get(*pos), Some(ArithToken::RParen)) {
                *pos += 1;
            }
            Ok(v)
        }
        _ => Ok(0),
    }
}

fn get_var_value(name: &str, state: &ShellState) -> i64 {
    state
        .get_var(name)
        .unwrap_or("0")
        .parse::<i64>()
        .unwrap_or(0)
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
    } else if dir_path.starts_with('~') {
        dirs::home_dir().unwrap_or_default().join(&dir_path[1..])
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

    for (i, c) in pattern.chars().enumerate() {
        match c {
            '(' if i > 0
                && matches!(
                    pattern.chars().nth(i.saturating_sub(1)),
                    Some('!' | '?' | '*' | '+' | '@')
                ) =>
            {
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
pub fn expand_conditional_words(words: &[Word], state: &mut ShellState) -> Vec<String> {
    state.expansion_error = None;
    let mut result = Vec::new();
    for word in words {
        result.push(expand_word_to_string(word, state));
        if state.expansion_error.is_some() {
            break;
        }
    }
    result
}

#[cfg(test)]
mod tests {
    use super::*;

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
        assert_eq!(
            expand_brace_range("1", "5", Some("0")),
            Vec::<String>::new()
        );
    }
}
