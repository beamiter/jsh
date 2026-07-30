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
        return fields;
    }

    // Check for "$@"/$@/$* which expand to multiple fields (positional params).
    if word_contains_at_star(word) {
        let fields = expand_parts_to_fields(word, state, false);
        // "$@" / "$*" with no positional params and no surrounding text → no words.
        if fields.len() == 1 && fields[0].is_empty() && state.positional_params.is_empty() {
            return Vec::new();
        }
        return fields;
    }

    // Check for brace expansion/range that produces multiple words
    for (i, part) in word.iter().enumerate() {
        let items = match part {
            WordPart::BraceExpansion(items) => Some(expand_brace_items(items, state)),
            WordPart::BraceRange { start, end, step } => {
                Some(expand_brace_range(start, end, step.as_deref()))
            }
            _ => None,
        };
        if let Some(items) = items {
            let mut results = Vec::new();
            for item in &items {
                let mut new_word: Word = Vec::new();
                for (j, p) in word.iter().enumerate() {
                    if j == i {
                        new_word.push(WordPart::Literal(item.clone()));
                    } else {
                        new_word.push(p.clone());
                    }
                }
                results.extend(expand_word(&new_word, state));
            }
            return results;
        }
    }

    // Expand parts while tracking which bytes came from unquoted expansions
    // (variables, command substitution, arithmetic). Only those bytes are
    // candidates for IFS word splitting.
    let (expanded, mask, has_unsplittable) = expand_word_masked(word, state);

    let mut fields = ifs_split(&expanded, &mask, state);
    if fields.is_empty() {
        if has_unsplittable {
            // e.g. `echo ""` or `echo "$empty"` → one empty field.
            fields.push(String::new());
        } else {
            // e.g. `echo $empty` → no fields at all.
            return Vec::new();
        }
    }

    // Globbing applies per resulting field, after word splitting.
    let mut out = Vec::new();
    for field in fields {
        out.extend(glob_field(&field, state));
    }
    out
}

/// Expand each part of a word, recording per-byte whether it originated from an
/// unquoted expansion (splittable) plus whether any unsplittable part was seen
/// (so an empty result can be distinguished: `echo ""` keeps one empty field,
/// `echo $empty` keeps none).
fn expand_word_masked(word: &Word, state: &mut ShellState) -> (String, Vec<bool>, bool) {
    let mut s = String::new();
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
        s.push_str(&text);
    }
    (s, mask, has_unsplittable)
}

/// Split a string into fields at splittable IFS characters. `mask[byte]` is true
/// where the byte came from an unquoted expansion. Honors bash IFS rules:
/// leading/trailing IFS whitespace is trimmed, runs of IFS whitespace delimit a
/// single field boundary, and each IFS non-whitespace character (with any
/// adjacent IFS whitespace) is one delimiter that can produce empty fields.
fn ifs_split(s: &str, mask: &[bool], state: &ShellState) -> Vec<String> {
    if s.is_empty() {
        return Vec::new();
    }
    let ifs = state
        .get_var("IFS")
        .map(|v| v.to_string())
        .unwrap_or_else(|| " \t\n".to_string());
    if ifs.is_empty() {
        // IFS empty → no word splitting at all.
        return vec![s.to_string()];
    }
    let is_ws = |c: char| (c == ' ' || c == '\t' || c == '\n') && ifs.contains(c);
    let is_ifs = |c: char| ifs.contains(c);

    let chars: Vec<(usize, char)> = s.char_indices().collect();
    let n = chars.len();
    let split_at = |i: usize| -> bool {
        let bp = chars[i].0;
        mask.get(bp).copied().unwrap_or(false) && is_ifs(chars[i].1)
    };

    let mut fields: Vec<String> = Vec::new();
    let mut field = String::new();
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
            field.push(chars[i].1);
            field_pending = true;
            i += 1;
        }
    }

    if field_pending || !field.is_empty() {
        fields.push(field);
    }
    fields
}

/// IFS-split a string that came entirely from an unquoted expansion.
fn ifs_split_unquoted(s: &str, state: &ShellState) -> Vec<String> {
    let mask = vec![true; s.len()];
    ifs_split(s, &mask, state)
}

/// Apply glob/extglob expansion to a single already-split field.
fn glob_field(expanded: &str, state: &mut ShellState) -> Vec<String> {
    let has_extglob = state.shell_opts.extglob && crate::glob_match::contains_extglob(expanded);

    if has_extglob {
        expand_with_extglob(expanded, state)
    } else if contains_glob(expanded) && !state.shell_opts.noglob {
        match glob::glob(expanded) {
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

/// Split `arr[@]` / `arr[*]` into (array name, subscript) when the name refers
/// to an existing array.
fn array_at_star_ref<'a>(name: &'a str, state: &ShellState) -> Option<(&'a str, &'a str)> {
    let bracket = name.find('[')?;
    let var_name = &name[..bracket];
    let subscript = &name[bracket + 1..name.len().saturating_sub(1)];
    if (subscript == "@" || subscript == "*") && state.is_array(var_name) {
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
                Some((var_name, _)) => state.array_values(var_name).is_empty(),
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
fn expand_parts_to_fields(parts: &[WordPart], state: &mut ShellState, quoted: bool) -> Vec<String> {
    let mut fields: Vec<String> = vec![String::new()];
    for part in parts {
        match part {
            WordPart::Variable(name) if name == "@" => {
                let params = state.positional_params.clone();
                append_fields(&mut fields, params);
            }
            WordPart::Variable(name) if name == "*" => {
                if quoted {
                    let sep = ifs_first(state);
                    let joined = state.positional_params.join(&sep);
                    fields.last_mut().unwrap().push_str(&joined);
                } else {
                    let params = state.positional_params.clone();
                    append_fields(&mut fields, params);
                }
            }
            WordPart::Variable(name) if array_at_star_ref(name, state).is_some() => {
                let (var_name, subscript) = array_at_star_ref(name, state).unwrap();
                let vals = state.array_values(var_name);
                if quoted {
                    if subscript == "*" {
                        let sep = ifs_first(state);
                        fields.last_mut().unwrap().push_str(&vals.join(&sep));
                    } else {
                        append_fields(&mut fields, vals);
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
                fields.last_mut().unwrap().push_str(&s);
            }
        }
    }
    fields
}

/// Merge additional fields: the first attaches to the current trailing field,
/// the rest become new fields. Empty input leaves `fields` untouched.
fn append_fields(fields: &mut Vec<String>, more: Vec<String>) {
    let mut iter = more.into_iter();
    if let Some(first) = iter.next() {
        fields.last_mut().unwrap().push_str(&first);
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
            format!("\x01rsh-closure:{}\x02", state.inline_closures.len() - 1)
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
    // ${#arr[@]} or ${#arr[*]} → array length
    if let Some(inner) = name.strip_prefix('#') {
        if let Some(bracket) = inner.find('[') {
            let var_name = &inner[..bracket];
            let subscript = &inner[bracket + 1..inner.len().saturating_sub(1)];
            if (subscript == "@" || subscript == "*") && state.is_array(var_name) {
                return state.array_length(var_name).to_string();
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
        if let Some(bracket) = inner.find('[') {
            let var_name = &inner[..bracket];
            let subscript = &inner[bracket + 1..inner.len().saturating_sub(1)];
            if (subscript == "@" || subscript == "*") && state.is_array(var_name) {
                return state.array_keys(var_name).join(" ");
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
            }

            // ${arr[idx]} - single element access
            if after_bracket.is_empty() && state.is_array(var_name) {
                return state
                    .get_array_element(var_name, subscript_part)
                    .unwrap_or_default();
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
        let step_abs = step
            .and_then(|s| s.parse::<i64>().ok().map(|v| v.abs()))
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
        if step_val > 0 {
            while i <= e {
                if needs_pad {
                    results.push(format!("{:0>width$}", i, width = pad_width));
                } else {
                    results.push(i.to_string());
                }
                i += step_val;
            }
        } else {
            while i >= e {
                if needs_pad {
                    results.push(format!("{:0>width$}", i, width = pad_width));
                } else {
                    results.push(i.to_string());
                }
                i += step_val;
            }
        }
        return results;
    }

    // Try character range
    if start.len() == 1 && end.len() == 1 {
        let s = start.chars().next().unwrap();
        let e = end.chars().next().unwrap();
        let step_abs = step
            .and_then(|s| s.parse::<i32>().ok().map(|v| v.abs()))
            .unwrap_or(1);
        if step_abs == 0 {
            return vec![];
        }
        let step_val = if s <= e { step_abs } else { -step_abs };

        let mut results = Vec::new();
        let mut i = s as i32;
        let end_i = e as i32;
        if step_val > 0 {
            while i <= end_i {
                if let Some(c) = char::from_u32(i as u32) {
                    results.push(c.to_string());
                }
                i += step_val;
            }
        } else {
            while i >= end_i {
                if let Some(c) = char::from_u32(i as u32) {
                    results.push(c.to_string());
                }
                i += step_val;
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

fn contains_glob(s: &str) -> bool {
    let mut escaped = false;
    let mut in_single = false;
    let mut in_double = false;
    for c in s.chars() {
        if escaped {
            escaped = false;
            continue;
        }
        match c {
            '\\' => escaped = true,
            '\'' if !in_double => in_single = !in_single,
            '"' if !in_single => in_double = !in_double,
            '*' | '?' | '[' if !in_single && !in_double => return true,
            _ => {}
        }
    }
    false
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
    } else {
        let expanded_dir = if dir_path.starts_with('~') {
            dirs::home_dir().unwrap_or_default().join(&dir_path[1..])
        } else {
            std::path::PathBuf::from(dir_path)
        };
        expanded_dir
    };

    let mut results = Vec::new();

    if let Ok(entries) = fs::read_dir(&search_dir) {
        for entry in entries.flatten() {
            if let Ok(path) = entry.path().canonicalize() {
                if let Some(filename) = path.file_name().and_then(|n| n.to_str()) {
                    // Apply dotglob filtering
                    if !state.shell_opts.dotglob && filename.starts_with('.') {
                        continue;
                    }

                    // Apply extglob matching
                    if crate::glob_match::extglob_match(&file_pattern, filename) {
                        results.push(path.to_string_lossy().to_string());
                    }
                }
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
