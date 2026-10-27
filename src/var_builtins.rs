//! The builtins that create, inspect and remove variables: `declare` (and its
//! synonym `typeset`), `local`, `export`, `readonly` and `unset`.
//!
//! They share one argument model. Options come first, `-x` turning an
//! attribute on and `+x` turning it off; every other argument is `name`,
//! `name=value`, `name+=value`, `name[sub]=value` or a compound assignment
//! `name=(...)`. A compound assignment the parser saw unquoted arrives as its
//! source text together with its index (see `SimpleCommand::compound_args`)
//! and is expanded element by element here, exactly like `name=(...)` on its
//! own; a quoted one (`declare -a 'xs=(1 2)'`) is only treated as compound for
//! an array, as in bash.

use crate::environment::{is_valid_identifier, ShellState};
use crate::variables::{
    declare_line, split_subscripted, ArrayKind, CompoundElement, Slot, VarAttrs, VarValue, Variable,
};

/// Which builtin is running; they differ in scope and defaults.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum DeclKind {
    /// `declare` / `typeset`: local inside a function unless `-g`.
    Declare,
    /// `local`: only inside a function.
    Local,
    /// `export`: the visible variable, exported.
    Export,
    /// `readonly`: the visible variable, made readonly.
    Readonly,
}

impl DeclKind {
    fn name(self) -> &'static str {
        match self {
            DeclKind::Declare => "declare",
            DeclKind::Local => "local",
            DeclKind::Export => "export",
            DeclKind::Readonly => "readonly",
        }
    }
}

/// Parsed options of a declaration builtin.
#[derive(Debug, Default)]
struct DeclOptions {
    on: VarAttrs,
    off: VarAttrs,
    print: bool,
    global: bool,
    functions: bool,
    function_names: bool,
    /// `local -I` / `declare -I`: a new local inherits the shadowed
    /// variable's value and attributes.
    inherit: bool,
}

/// Run `declare`, `typeset`, `local`, `export` or `readonly` with already
/// expanded `args`. `compound` lists the indices of `args` that are unexpanded
/// compound assignments written as such in the source.
pub fn run_declaration(
    name: &str,
    args: &[String],
    compound: &[usize],
    state: &mut ShellState,
) -> i32 {
    match name {
        "declare" | "typeset" => declaration(DeclKind::Declare, args, compound, state),
        "local" => declaration(DeclKind::Local, args, compound, state),
        "export" => declaration(DeclKind::Export, args, compound, state),
        "readonly" => declaration(DeclKind::Readonly, args, compound, state),
        "unset" => unset(args, state),
        _ => crate::builtins::run_builtin(name, args, state),
    }
}

pub fn declare(args: &[String], state: &mut ShellState) -> i32 {
    declaration(DeclKind::Declare, args, &[], state)
}

pub fn local(args: &[String], state: &mut ShellState) -> i32 {
    declaration(DeclKind::Local, args, &[], state)
}

pub fn export(args: &[String], state: &mut ShellState) -> i32 {
    declaration(DeclKind::Export, args, &[], state)
}

pub fn readonly(args: &[String], state: &mut ShellState) -> i32 {
    declaration(DeclKind::Readonly, args, &[], state)
}

fn usage_error(kind: DeclKind, option: &str) -> i32 {
    eprintln!("jsh: {}: {}: invalid option", kind.name(), option);
    let usage = match kind {
        DeclKind::Declare => "declare [-aAfFgiIlnrtux] [-p] [name[=value] ...]",
        DeclKind::Local => "local [option] name[=value] ...",
        DeclKind::Export => "export [-fn] [name[=value] ...] or export -p",
        DeclKind::Readonly => "readonly [-aAf] [name[=value] ...] or readonly -p",
    };
    eprintln!("{}: usage: {}", kind.name(), usage);
    2
}

/// Parse leading options; returns the options and the index of the first
/// operand, or the exit status of a usage error.
fn parse_options(kind: DeclKind, args: &[String]) -> Result<(DeclOptions, usize), i32> {
    let mut opts = DeclOptions::default();
    let allowed = match kind {
        DeclKind::Declare | DeclKind::Local => "aAfFgiIlnrtuxcp",
        DeclKind::Export => "fnp",
        DeclKind::Readonly => "aAfp",
    };
    let mut i = 0;
    while i < args.len() {
        let arg = args[i].as_str();
        if arg == "--" {
            i += 1;
            break;
        }
        // A lone `-` is `local -`, whose option-scoping belongs to the
        // executor; accept it here so it never declares a variable named `-`.
        // (It must be checked before the option split below, which stops at
        // any one-character argument.)
        if arg == "-" {
            i += 1;
            continue;
        }
        let (turn_on, letters) = match arg.as_bytes().first() {
            Some(b'-') if arg.len() > 1 => (true, &arg[1..]),
            Some(b'+') if arg.len() > 1 => (false, &arg[1..]),
            _ => break,
        };
        let mut case_on = 0;
        for letter in letters.chars() {
            if !allowed.contains(letter) {
                return Err(usage_error(kind, &format!("{}{}", &arg[..1], letter)));
            }
            match (kind, letter) {
                (_, 'p') => opts.print = turn_on,
                (_, 'g') => opts.global = turn_on,
                (_, 'I') => opts.inherit = turn_on,
                (_, 'F') => {
                    opts.functions = true;
                    opts.function_names = true;
                }
                (_, 'f') => opts.functions = true,
                // `export -n` removes the export attribute.
                (DeclKind::Export, 'n') => opts.off.insert(VarAttrs::EXPORT),
                (_, letter) => {
                    let attr = VarAttrs::from_letter(letter).expect("letter is allowed");
                    if turn_on {
                        if attr.intersects(VarAttrs::CASE) {
                            case_on += 1;
                        }
                        opts.on.insert(attr);
                        opts.off.remove(attr);
                    } else {
                        opts.off.insert(attr);
                        opts.on.remove(attr);
                    }
                }
            }
        }
        // Bash keeps at most one case attribute; asking for two of them at
        // once (`declare -lu`) leaves the variable with neither.
        if case_on > 1 {
            opts.on.remove(VarAttrs::CASE);
        }
        i += 1;
    }
    match kind {
        DeclKind::Export => opts.on.insert(VarAttrs::EXPORT),
        DeclKind::Readonly => opts.on.insert(VarAttrs::READONLY),
        _ => {}
    }
    // `export -n` wins over the implied `-x`.
    if opts.off.contains(VarAttrs::EXPORT) {
        opts.on.remove(VarAttrs::EXPORT);
    }
    Ok((opts, i))
}

fn declaration(kind: DeclKind, args: &[String], compound: &[usize], state: &mut ShellState) -> i32 {
    let (opts, first) = match parse_options(kind, args) {
        Ok(parsed) => parsed,
        Err(status) => return status,
    };
    let operands = &args[first..];

    if kind == DeclKind::Local && !state.in_function() {
        eprintln!("jsh: local: can only be used in a function");
        return 1;
    }

    if opts.functions {
        return declare_functions(kind, &opts, operands, state);
    }

    if operands.is_empty() {
        return list_variables(kind, &opts, state);
    }

    if opts.print {
        return print_variables(kind, operands, state);
    }

    let mut status = 0;
    for (offset, operand) in operands.iter().enumerate() {
        let is_compound = compound.contains(&(first + offset));
        if declare_one(kind, &opts, operand, is_compound, state).is_err() {
            status = 1;
        }
    }
    status
}

/// `declare -f` / `-F`: print function definitions (through the same AST
/// printer as `type`) or just their names. `export -f` and `readonly -f`
/// accept existing names without printing; jsh does not export or protect
/// function definitions.
fn declare_functions(
    kind: DeclKind,
    opts: &DeclOptions,
    operands: &[String],
    state: &mut ShellState,
) -> i32 {
    let prints = matches!(kind, DeclKind::Declare);
    let show = |name: &str, body: &crate::parser::ast::CompoundCommand| {
        if opts.function_names {
            println!("{name}");
        } else {
            println!("{}", crate::parser::unparse::format_function(name, body));
        }
    };
    if operands.is_empty() {
        if prints || opts.function_names {
            let mut names: Vec<&String> = state.functions.keys().collect();
            names.sort();
            for name in names {
                if opts.function_names {
                    println!("declare -f {name}");
                } else {
                    show(name, &state.functions[name]);
                }
            }
        }
        return 0;
    }
    let mut status = 0;
    for name in operands {
        match state.functions.get(name) {
            Some(body) if prints => show(name, body),
            Some(_) => {}
            None => status = 1,
        }
    }
    status
}

/// Does `var` pass the attribute filter of `declare -x` / `readonly -p` style
/// listings?
fn matches_filter(var: &Variable, on: VarAttrs) -> bool {
    let mut attrs = var.attrs;
    match var.value {
        VarValue::Indexed(_) => attrs.insert(VarAttrs::ARRAY),
        VarValue::Assoc(_) => attrs.insert(VarAttrs::ASSOC),
        _ => {}
    }
    attrs.contains(on)
}

/// `declare`, `declare -p`, `export`, `readonly` and friends with no names.
fn list_variables(kind: DeclKind, opts: &DeclOptions, state: &ShellState) -> i32 {
    // Bare `declare`/`typeset`/`local` print `name=value` lines like `set`.
    let set_style =
        !opts.print && opts.on.is_empty() && matches!(kind, DeclKind::Declare | DeclKind::Local);
    let local_only = kind == DeclKind::Local;
    for (name, slot, var) in state.visible_variables() {
        // Environment entries whose names are not identifiers (exported
        // functions, `BASH_FUNC_f%%`) are passed through, never listed.
        if !is_valid_identifier(name) {
            continue;
        }
        if local_only && !matches!(slot, Slot::Local(i) if Some(i) == state.function_scope()) {
            continue;
        }
        if !matches_filter(var, opts.on) {
            continue;
        }
        if set_style {
            if let Some(line) = set_style_line(name, var) {
                println!("{line}");
            }
        } else {
            println!("{}", declare_line(name, var));
        }
    }
    0
}

/// One line of `set` / bare `declare` output: `name=value` with the value
/// quoted the way bash's `set` quotes it, `name=(...)` for arrays.
pub fn set_style_line(name: &str, var: &Variable) -> Option<String> {
    match &var.value {
        VarValue::Unset => None,
        VarValue::Scalar(s) => Some(format!("{}={}", name, quote_for_set(s))),
        array => Some(format!(
            "{}={}",
            name,
            crate::variables::array_assignment_body(array)
        )),
    }
}

/// Bash's `set` output quoting: bare when nothing needs protecting, `'...'`
/// otherwise, `$'...'` for control characters.
fn quote_for_set(value: &str) -> String {
    let plain = !value.is_empty()
        && value
            .chars()
            .all(|c| c.is_alphanumeric() || "_-./:,+%@=".contains(c));
    if plain {
        value.to_string()
    } else {
        crate::variables::single_quote_value(value)
    }
}

/// `declare -p NAME...` (and `export -p NAME`, `readonly -p NAME`).
fn print_variables(kind: DeclKind, names: &[String], state: &ShellState) -> i32 {
    let mut status = 0;
    for name in names {
        let found = state.find_variable(name).filter(|var| {
            // `export -p NAME` only shows NAME if it is exported, and so on.
            kind == DeclKind::Declare
                || kind == DeclKind::Local
                || matches_filter(var, opts_filter(kind))
        });
        match found {
            Some(var) => println!("{}", declare_line(name, var)),
            None => {
                if matches!(kind, DeclKind::Declare | DeclKind::Local) {
                    eprintln!("jsh: {}: {}: not found", kind.name(), name);
                    status = 1;
                }
            }
        }
    }
    status
}

fn opts_filter(kind: DeclKind) -> VarAttrs {
    match kind {
        DeclKind::Export => VarAttrs::EXPORT,
        DeclKind::Readonly => VarAttrs::READONLY,
        _ => VarAttrs::NONE,
    }
}

/// The parts of one operand.
struct Operand<'a> {
    name: &'a str,
    subscript: Option<&'a str>,
    append: bool,
    value: Option<&'a str>,
}

fn split_operand(text: &str) -> Operand<'_> {
    match crate::parser::parse::split_assignment_word(text) {
        Some((lhs, append, value)) => {
            let (name, subscript) = match split_subscripted(lhs) {
                Some((name, sub)) => (name, Some(sub)),
                None => (lhs, None),
            };
            Operand {
                name,
                subscript,
                append,
                value: Some(value),
            }
        }
        None => Operand {
            name: text,
            subscript: None,
            append: false,
            value: None,
        },
    }
}

/// Report an error for one operand; the builtin's status becomes 1.
fn fail(kind: DeclKind, message: String) -> Result<(), ()> {
    eprintln!("jsh: {}: {}", kind.name(), message);
    Err(())
}

/// Find (or create) the variable a declaration operates on.
fn declaration_slot(
    kind: DeclKind,
    opts: &DeclOptions,
    name: &str,
    state: &mut ShellState,
) -> Result<Slot, String> {
    if opts.global {
        return Ok(state.declare_global(name));
    }
    let make_local = matches!(kind, DeclKind::Declare | DeclKind::Local) && state.in_function();
    if make_local {
        let existed = state
            .function_scope()
            .is_some_and(|scope| state.local_vars_stack[scope].vars.contains_key(name));
        let inherited = if opts.inherit && !existed {
            state.find_variable(name).cloned()
        } else {
            None
        };
        let slot = state.declare_local(name)?;
        if let Some(outer) = inherited {
            if let Some(var) = state.var_at_mut(slot, name) {
                var.value = outer.value;
                let mut attrs = outer.attrs;
                attrs.remove(VarAttrs::NAMEREF.union(VarAttrs::READONLY));
                var.attrs = attrs;
            }
        }
        return Ok(slot);
    }
    Ok(match state.slot_of(name) {
        Some(slot) => slot,
        None => state.declare_global(name),
    })
}

fn declare_one(
    kind: DeclKind,
    opts: &DeclOptions,
    text: &str,
    is_compound: bool,
    state: &mut ShellState,
) -> Result<(), ()> {
    let operand = split_operand(text);
    if !is_valid_identifier(operand.name) {
        return fail(kind, format!("`{text}': not a valid identifier"));
    }
    let mut name = operand.name.to_string();

    // A declaration of an existing nameref acts on the variable it names,
    // unless the nameref attribute itself is being changed.
    let changes_nameref = opts.on.union(opts.off).contains(VarAttrs::NAMEREF);
    if !changes_nameref {
        let current = state.find_variable(&name);
        let in_reach = current.is_some_and(|v| v.attrs.contains(VarAttrs::NAMEREF));
        if in_reach {
            match state.resolve_nameref(&name) {
                Ok(target) if target.subscript.is_none() => name = target.name,
                Ok(_) => {}
                Err(message) => return fail(kind, message),
            }
        }
    }

    let slot = match declaration_slot(kind, opts, &name, state) {
        Ok(slot) => slot,
        Err(message) => {
            // `local` outside a function was rejected up front; this is a
            // readonly global the function tried to shadow.
            return fail(kind, message);
        }
    };
    let current = state.var_at(slot, &name).cloned().unwrap_or_default();

    // A readonly variable keeps its value and its readonly attribute; bash
    // still lets `declare -x` or a repeated `readonly` through.
    if current.attrs.contains(VarAttrs::READONLY)
        && (operand.value.is_some() || opts.off.contains(VarAttrs::READONLY))
    {
        return fail(kind, format!("{name}: readonly variable"));
    }

    // Array kind changes bash refuses.
    if opts.on.contains(VarAttrs::ARRAY) && current.is_assoc() {
        return fail(
            kind,
            format!("{name}: cannot convert associative to indexed array"),
        );
    }
    if opts.on.contains(VarAttrs::ASSOC) && current.is_indexed() {
        return fail(
            kind,
            format!("{name}: cannot convert indexed to associative array"),
        );
    }
    if opts.off.intersects(VarAttrs::ARRAY.union(VarAttrs::ASSOC)) && current.is_array() {
        return fail(
            kind,
            format!("{name}: cannot destroy array variables in this way"),
        );
    }

    // A nameref names a variable: validate the reference.
    if opts.on.contains(VarAttrs::NAMEREF) {
        if let Some(target) = operand.value {
            let valid = is_valid_identifier(target) || split_subscripted(target).is_some();
            if !valid {
                return fail(
                    kind,
                    format!("`{target}': invalid variable name for name reference"),
                );
            }
            if target == name {
                return fail(
                    kind,
                    format!("{name}: nameref variable self references not allowed"),
                );
            }
        }
    }

    // Attributes first, so that `declare -i n=1+1` evaluates its value.
    {
        let var = state
            .var_at_mut(slot, &name)
            .expect("declaration_slot created it");
        let mut on = opts.on;
        let off = opts.off;
        // READONLY is applied after the value is in place.
        on.remove(VarAttrs::READONLY);
        if on.intersects(VarAttrs::CASE) {
            var.attrs.remove(VarAttrs::CASE);
        }
        var.attrs.insert(on);
        var.attrs.remove(off);
        // `declare -a x` on a scalar makes it element 0; `-A` makes it key "0".
        match (
            &var.value,
            on.contains(VarAttrs::ARRAY),
            on.contains(VarAttrs::ASSOC),
        ) {
            (VarValue::Scalar(s), true, _) => {
                var.value =
                    VarValue::Indexed(crate::variables::IndexedArray::from_values(vec![s.clone()]));
            }
            (VarValue::Scalar(s), _, true) => {
                let mut m = crate::variables::AssocArray::new();
                m.insert("0".to_string(), s.clone());
                var.value = VarValue::Assoc(m);
            }
            _ => {}
        }
    }

    let assigned = assign_operand(kind, opts, &name, slot, &operand, text, is_compound, state);

    if opts.on.contains(VarAttrs::READONLY) {
        if let Some(var) = state.var_at_mut(slot, &name) {
            var.attrs.insert(VarAttrs::READONLY);
        }
    }
    state.sync_env(&name);
    assigned
}

/// Assign the operand's value (if any) to the declared variable.
#[allow(clippy::too_many_arguments)]
fn assign_operand(
    kind: DeclKind,
    opts: &DeclOptions,
    name: &str,
    slot: Slot,
    operand: &Operand<'_>,
    text: &str,
    is_compound: bool,
    state: &mut ShellState,
) -> Result<(), ()> {
    let Some(value) = operand.value else {
        return Ok(());
    };
    let var = state.var_at(slot, name).cloned().unwrap_or_default();

    // A nameref's own value is the name it refers to.
    if var.attrs.contains(VarAttrs::NAMEREF) && operand.subscript.is_none() {
        if let Some(var) = state.var_at_mut(slot, name) {
            var.value = VarValue::Scalar(value.to_string());
        }
        return Ok(());
    }

    let compound_syntax = value.starts_with('(') && value.ends_with(')');
    let as_compound = operand.subscript.is_none()
        && (is_compound
            || (compound_syntax
                && (var.is_array() || opts.on.intersects(VarAttrs::ARRAY.union(VarAttrs::ASSOC)))));

    if as_compound {
        let Some(parsed) = crate::parser::parse::split_compound_assignment(text) else {
            return fail(kind, format!("{name}: bad compound assignment"));
        };
        let elements: Vec<CompoundElement> =
            crate::expand::expand_compound_elements(&parsed.elements, state);
        let array_kind = if var.is_assoc() {
            ArrayKind::Assoc
        } else {
            ArrayKind::Indexed
        };
        state.assignment_failed = false;
        return match state.assign_compound(
            name,
            Some(slot),
            elements,
            operand.append,
            Some(array_kind),
        ) {
            Ok(()) => Ok(()),
            Err(_) => Err(()),
        };
    }

    if let Some(subscript) = operand.subscript {
        let key = crate::expand::expand_subscript_text(subscript, state);
        return if state.assign_element(name, &key, value, operand.append) {
            Ok(())
        } else {
            Err(())
        };
    }

    if state.assign_scalar_at(slot, name, value, operand.append) {
        Ok(())
    } else {
        Err(())
    }
}

/// `unset [-fvn] name...`.
pub fn unset(args: &[String], state: &mut ShellState) -> i32 {
    let mut functions = false;
    let mut variables = false;
    let mut nameref = false;
    let mut i = 0;
    while i < args.len() {
        let arg = args[i].as_str();
        if arg == "--" {
            i += 1;
            break;
        }
        if !arg.starts_with('-') || arg.len() < 2 {
            break;
        }
        for letter in arg[1..].chars() {
            match letter {
                'f' => functions = true,
                'v' => variables = true,
                'n' => nameref = true,
                _ => {
                    eprintln!("jsh: unset: -{letter}: invalid option");
                    eprintln!("unset: usage: unset [-f] [-v] [-n] [name ...]");
                    return 2;
                }
            }
        }
        i += 1;
    }

    let mut status = 0;
    for name in &args[i..] {
        if functions && !variables {
            state.functions.remove(name);
            continue;
        }
        let (base, subscript) = match split_subscripted(name) {
            Some((base, sub)) => (base, Some(sub)),
            None => (name.as_str(), None),
        };
        if !is_valid_identifier(base) {
            // Bash only complains when the kind was pinned with `-v`/`-f`; a
            // bare `unset a=b` is a silent no-op.
            if variables {
                eprintln!("jsh: unset: `{name}': not a valid identifier");
                status = 1;
            }
            continue;
        }
        let result = match subscript {
            Some(sub) => {
                // `unset 'a[$i]'` expands the subscript itself, as bash does.
                let key = crate::expand::expand_subscript_text(sub, state);
                let target = state
                    .resolve_nameref(base)
                    .map(|t| t.name)
                    .unwrap_or_else(|_| base.to_string());
                state.unset_element(&target, &key)
            }
            None => {
                let exists = state.find_variable(base).is_some();
                if !exists && !variables && state.functions.contains_key(base) {
                    // With neither -v nor -f, a name that is no variable
                    // unsets the function of that name.
                    state.functions.remove(base);
                    continue;
                }
                if nameref {
                    state.unset_exact(base)
                } else {
                    state.unset_variable(base)
                }
            }
        };
        if let Err(message) = result {
            eprintln!("jsh: unset: {message}");
            status = 1;
        }
    }
    status
}
