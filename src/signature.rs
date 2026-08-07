/// Phase 14b — command signatures.
///
/// Each value-aware builtin can declare a `Signature` describing its
/// expected pipeline input type, output type, and positional parameters.
/// Signatures power `help <cmd>` and `help -r <cmd>` (record form).
/// Runtime arg-validation is intentionally deferred — these are
/// declarative docs for now.
///
/// New entries: add a static `PARAMS_*` slice and an entry at the bottom
/// of `SIGNATURES`. Cheap to extend.
use indexmap::IndexMap;
use once_cell::sync::Lazy;
use std::collections::HashMap;

use crate::value::Value;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Type {
    Any,
    Null,
    Bool,
    Int,
    Float,
    Number,
    String,
    List,
    Record,
    Table,
    Binary,
    Closure,
    Stream,
    Path,
    Union(&'static [Type]),
}

impl Type {
    pub fn name(self) -> &'static str {
        match self {
            Type::Any => "any",
            Type::Null => "nothing",
            Type::Bool => "bool",
            Type::Int => "int",
            Type::Float => "float",
            Type::Number => "number",
            Type::String => "string",
            Type::List => "list",
            Type::Record => "record",
            Type::Table => "table",
            Type::Binary => "binary",
            Type::Closure => "closure",
            Type::Stream => "stream",
            Type::Path => "path",
            Type::Union(_) => "union",
        }
    }

    pub fn render(self) -> String {
        match self {
            Type::Union(parts) => parts
                .iter()
                .map(|t| t.name())
                .collect::<Vec<_>>()
                .join(" | "),
            other => other.name().to_string(),
        }
    }
}

#[derive(Debug, Clone, Copy)]
pub struct Param {
    pub name: &'static str,
    pub kind: Type,
    pub optional: bool,
    pub rest: bool,
}

const fn p(name: &'static str, kind: Type) -> Param {
    Param {
        name,
        kind,
        optional: false,
        rest: false,
    }
}
const fn p_opt(name: &'static str, kind: Type) -> Param {
    Param {
        name,
        kind,
        optional: true,
        rest: false,
    }
}
const fn p_rest(name: &'static str, kind: Type) -> Param {
    Param {
        name,
        kind,
        optional: false,
        rest: true,
    }
}

#[derive(Debug, Clone, Copy)]
pub struct Signature {
    pub name: &'static str,
    pub input: Type,
    pub output: Type,
    pub params: &'static [Param],
    pub desc: &'static str,
}

impl Signature {
    /// Render the signature as a Record so it can flow through pipelines.
    pub fn to_record(&self) -> Value {
        let mut m = IndexMap::new();
        m.insert("name".to_string(), Value::String(self.name.to_string()));
        m.insert("input".to_string(), Value::String(self.input.render()));
        m.insert("output".to_string(), Value::String(self.output.render()));
        m.insert("desc".to_string(), Value::String(self.desc.to_string()));
        let params: Vec<Value> = self
            .params
            .iter()
            .map(|p| {
                let mut pm = IndexMap::new();
                pm.insert("name".to_string(), Value::String(p.name.to_string()));
                pm.insert("type".to_string(), Value::String(p.kind.render()));
                pm.insert("optional".to_string(), Value::Bool(p.optional));
                pm.insert("rest".to_string(), Value::Bool(p.rest));
                Value::Record(pm)
            })
            .collect();
        m.insert("params".to_string(), Value::List(params));
        Value::Record(m)
    }

    /// Validate the arity of a positional argument list against this signature.
    ///
    /// Returns Err(message) if the user gave fewer args than there are
    /// required positionals, or supplied too many when there's no rest.
    /// The check is intentionally arity-only — type inspection happens
    /// inside each builtin where the runtime value is available.
    ///
    /// Args that start with `-` are treated as flags and skipped, so
    /// callers can mix flags with positionals freely.
    pub fn validate_args(&self, args: &[String]) -> Result<(), String> {
        // Walk left-to-right, skipping flags. We can't know which flags
        // consume an argument without per-builtin schemas, so as soon as
        // we see *any* short/long flag we stop enforcing the upper bound
        // — the builtin itself owns flag parsing. The lower bound is
        // counted only from leading positionals (before any flag).
        let mut leading_positional = 0usize;
        let mut saw_flag = false;
        let mut total_positional = 0usize;
        for a in args {
            if a.starts_with('-') && a.len() > 1 && !a.chars().nth(1).unwrap().is_ascii_digit() {
                saw_flag = true;
            } else {
                total_positional += 1;
                if !saw_flag {
                    leading_positional += 1;
                }
            }
        }

        let required = self
            .params
            .iter()
            .take_while(|p| !p.optional && !p.rest)
            .count();
        let has_rest = self.params.iter().any(|p| p.rest);
        let max = if has_rest {
            usize::MAX
        } else {
            self.params.len()
        };

        // Lower bound: enough leading positionals OR the user gave a flag
        // (in which case the builtin may be in a help/short-form mode).
        if !saw_flag && leading_positional < required {
            let names: Vec<&str> = self.params.iter().take(required).map(|p| p.name).collect();
            return Err(format!(
                "{}: missing required arg `{}` (expected {}: {})",
                self.name,
                names.get(leading_positional).copied().unwrap_or("?"),
                required,
                names.join(", "),
            ));
        }
        // Upper bound: only enforce when there are no flags to confuse us.
        if !saw_flag && total_positional > max {
            return Err(format!(
                "{}: too many args (expected at most {}, got {})",
                self.name, max, total_positional
            ));
        }
        Ok(())
    }

    pub fn render_help(&self) -> String {
        let mut out = String::new();
        out.push_str(&format!(
            "{} :: {} -> {}\n",
            self.name,
            self.input.render(),
            self.output.render(),
        ));
        out.push_str(&format!("  {}\n", self.desc));
        if !self.params.is_empty() {
            out.push_str("\nParameters:\n");
            for p in self.params {
                let tag = if p.rest {
                    "..."
                } else if p.optional {
                    "?"
                } else {
                    ""
                };
                out.push_str(&format!("  {}{} : {}\n", tag, p.name, p.kind.render()));
            }
        }
        out
    }
}

// ---------------------------------------------------------------------------
// Static parameter slices
// ---------------------------------------------------------------------------

const NUMBER_OR_STR: &[Type] = &[Type::Number, Type::String];
const CLOSURE_OR_STR: &[Type] = &[Type::Closure, Type::String];

const PARAMS_WHERE: &[Param] = &[p("pred", Type::Union(CLOSURE_OR_STR))];
const PARAMS_EACH: &[Param] = &[p("closure", Type::Closure)];
const PARAMS_REDUCE: &[Param] = &[p_opt("init", Type::Any), p("closure", Type::Closure)];
const PARAMS_PREDICATE: &[Param] = &[p("closure", Type::Closure)];
const PARAMS_FIRST: &[Param] = &[p_opt("n", Type::Int)];
const PARAMS_TAKEN: &[Param] = &[p("n", Type::Int)];
const PARAMS_DROPN: &[Param] = &[p_opt("n", Type::Int)];
const PARAMS_GET: &[Param] = &[p_rest("path", Type::String)];
const PARAMS_SELECT: &[Param] = &[p_rest("cols", Type::String)];
const PARAMS_SORT_BY: &[Param] = &[p_rest("fields", Type::String)];
const PARAMS_GROUP_BY: &[Param] = &[p("field", Type::String)];
const PARAMS_RENAME: &[Param] = &[p_rest("pairs", Type::String)];
const PARAMS_SPLIT: &[Param] = &[p("sep_or_sub", Type::String), p_rest("args", Type::Any)];
const PARAMS_PARSE: &[Param] = &[p("template", Type::String)];
const PARAMS_STR: &[Param] = &[p("op", Type::String), p_rest("args", Type::Any)];
const PARAMS_FORMAT: &[Param] = &[p("template", Type::String)];
const PARAMS_MATH: &[Param] = &[p("op", Type::String), p_rest("cols", Type::String)];
const PARAMS_RANGE: &[Param] = &[p("spec", Type::Union(NUMBER_OR_STR))];
const PARAMS_DO: &[Param] = &[p("closure", Type::Closure), p_rest("args", Type::Any)];
const PARAMS_TRY: &[Param] = &[
    p("closure", Type::Closure),
    p_opt("catch", Type::Any),
    p_opt("handler", Type::Closure),
];
const PARAMS_ERROR: &[Param] = &[p("subcmd", Type::String), p_opt("msg", Type::String)];
const PARAMS_HELP: &[Param] = &[p_opt("cmd", Type::String)];

// Phase 15a — additional parameter slices
const PARAMS_PATH_STR: &[Param] = &[p_opt("path", Type::String)];
const PARAMS_REQ_PATH: &[Param] = &[p("path", Type::String)];
const PARAMS_FIELD_VAL: &[Param] = &[p("field", Type::String), p("value", Type::Any)];
const PARAMS_NAME: &[Param] = &[p("name", Type::String)];
const PARAMS_INTO: &[Param] = &[p("type", Type::String), p_rest("cols", Type::String)];
const PARAMS_ZIP: &[Param] = &[p("other", Type::List)];
const PARAMS_MOVE: &[Param] = &[p_rest("args", Type::String)];
const PARAMS_MERGE: &[Param] = &[p("other", Type::Record)];
const PARAMS_SUBOP: &[Param] = &[p("op", Type::String), p_rest("args", Type::Any)];
const PARAMS_VALUE: &[Param] = &[p("value", Type::Any), p_rest("cols", Type::String)];
const PARAMS_N: &[Param] = &[p("n", Type::Int)];
const PARAMS_SPLIT_BY: &[Param] = &[p("by", Type::Union(CLOSURE_OR_STR))];
const PARAMS_SCHEME: &[Param] = &[p("scheme", Type::String)];
const PARAMS_ITEMS: &[Param] = &[p_rest("items", Type::Any)];
const PARAMS_CHAR: &[Param] = &[p("name", Type::String)];

// ---------------------------------------------------------------------------
// Signature registry
// ---------------------------------------------------------------------------

const SIGS: &[Signature] = &[
    // structural transforms
    Signature {
        name: "from-json",
        input: Type::Stream,
        output: Type::Any,
        params: &[],
        desc: "Parse JSON / NDJSON bytes into typed values.",
    },
    Signature {
        name: "from-ndjson",
        input: Type::Stream,
        output: Type::Stream,
        params: &[],
        desc: "Parse NDJSON lazily (one JSON value per line) as a value stream.",
    },
    Signature {
        name: "to-json",
        input: Type::Any,
        output: Type::Stream,
        params: &[],
        desc: "Serialize values to pretty JSON bytes.",
    },
    Signature {
        name: "from-yaml",
        input: Type::Stream,
        output: Type::Any,
        params: &[],
        desc: "Parse YAML bytes into typed values.",
    },
    Signature {
        name: "to-yaml",
        input: Type::Any,
        output: Type::Stream,
        params: &[],
        desc: "Serialize values to YAML bytes.",
    },
    Signature {
        name: "from-toml",
        input: Type::Stream,
        output: Type::Record,
        params: &[],
        desc: "Parse TOML bytes into a record.",
    },
    Signature {
        name: "to-toml",
        input: Type::Record,
        output: Type::Stream,
        params: &[],
        desc: "Serialize a record to TOML bytes.",
    },
    Signature {
        name: "from-csv",
        input: Type::Stream,
        output: Type::Table,
        params: &[],
        desc: "Parse CSV bytes into a table.",
    },
    Signature {
        name: "to-csv",
        input: Type::Table,
        output: Type::Stream,
        params: &[],
        desc: "Serialize a table to CSV bytes.",
    },
    Signature {
        name: "to-table",
        input: Type::Any,
        output: Type::Stream,
        params: &[],
        desc: "Render values as an aligned table.",
    },
    // filtering / iteration
    Signature {
        name: "where",
        input: Type::List,
        output: Type::List,
        params: PARAMS_WHERE,
        desc: "Filter rows by closure or `<field> <op> <value>`.",
    },
    Signature {
        name: "each",
        input: Type::List,
        output: Type::List,
        params: PARAMS_EACH,
        desc: "Map each item through a closure. -k keeps Null results.",
    },
    Signature {
        name: "par-each",
        input: Type::List,
        output: Type::List,
        params: PARAMS_EACH,
        desc: "Like each, but runs pure-expression closures in parallel.",
    },
    Signature {
        name: "reduce",
        input: Type::List,
        output: Type::Any,
        params: PARAMS_REDUCE,
        desc: "Fold the list with an initial value and a 2-arg closure.",
    },
    Signature {
        name: "any",
        input: Type::List,
        output: Type::Bool,
        params: PARAMS_PREDICATE,
        desc: "True if any item satisfies the closure.",
    },
    Signature {
        name: "all",
        input: Type::List,
        output: Type::Bool,
        params: PARAMS_PREDICATE,
        desc: "True if every item satisfies the closure.",
    },
    Signature {
        name: "first",
        input: Type::List,
        output: Type::Any,
        params: PARAMS_FIRST,
        desc: "First N items (default 1).",
    },
    Signature {
        name: "last",
        input: Type::List,
        output: Type::Any,
        params: PARAMS_FIRST,
        desc: "Last N items (default 1).",
    },
    Signature {
        name: "take",
        input: Type::List,
        output: Type::List,
        params: PARAMS_TAKEN,
        desc: "Take the first N items.",
    },
    Signature {
        name: "skip",
        input: Type::List,
        output: Type::List,
        params: PARAMS_TAKEN,
        desc: "Skip the first N items.",
    },
    Signature {
        name: "drop",
        input: Type::List,
        output: Type::List,
        params: PARAMS_DROPN,
        desc: "Drop the last N items (default 1).",
    },
    // records / tables
    Signature {
        name: "get",
        input: Type::Any,
        output: Type::Any,
        params: PARAMS_GET,
        desc: "Project a field path. -i returns Null on miss.",
    },
    Signature {
        name: "select",
        input: Type::Table,
        output: Type::Table,
        params: PARAMS_SELECT,
        desc: "Keep only the listed columns.",
    },
    Signature {
        name: "reject",
        input: Type::Table,
        output: Type::Table,
        params: PARAMS_SELECT,
        desc: "Drop the listed columns.",
    },
    Signature {
        name: "sort-by",
        input: Type::Table,
        output: Type::Table,
        params: PARAMS_SORT_BY,
        desc: "Sort rows ascending by one or more fields.",
    },
    Signature {
        name: "group-by",
        input: Type::Table,
        output: Type::Record,
        params: PARAMS_GROUP_BY,
        desc: "Group rows into a record keyed by field value.",
    },
    Signature {
        name: "rename",
        input: Type::Table,
        output: Type::Table,
        params: PARAMS_RENAME,
        desc: "Rename columns: rename old new [old2 new2 ...].",
    },
    // text
    Signature {
        name: "lines",
        input: Type::Stream,
        output: Type::List,
        params: &[],
        desc: "Split input bytes on newline.",
    },
    Signature {
        name: "split",
        input: Type::String,
        output: Type::List,
        params: PARAMS_SPLIT,
        desc: "Split a string on a separator.",
    },
    Signature {
        name: "parse",
        input: Type::String,
        output: Type::Table,
        params: PARAMS_PARSE,
        desc: "Parse each line with a template like `{a} {b}`.",
    },
    Signature {
        name: "str",
        input: Type::String,
        output: Type::String,
        params: PARAMS_STR,
        desc: "String subcommand (upcase/downcase/trim/...).",
    },
    Signature {
        name: "format",
        input: Type::Any,
        output: Type::String,
        params: PARAMS_FORMAT,
        desc: "Format a string template `{field}` against records.",
    },
    // numeric
    Signature {
        name: "math",
        input: Type::List,
        output: Type::Number,
        params: PARAMS_MATH,
        desc: "Reductions: sum/avg/min/max/median/product/etc.",
    },
    Signature {
        name: "range",
        input: Type::Null,
        output: Type::List,
        params: PARAMS_RANGE,
        desc: "Generate an integer range. Args: `start..end` or `n`.",
    },
    // control flow
    Signature {
        name: "do",
        input: Type::Any,
        output: Type::Any,
        params: PARAMS_DO,
        desc: "Run a closure with the pipeline input as its argument.",
    },
    Signature {
        name: "try",
        input: Type::Any,
        output: Type::Any,
        params: PARAMS_TRY,
        desc: "Run a closure; on error optionally invoke catch.",
    },
    Signature {
        name: "error",
        input: Type::Any,
        output: Type::Any,
        params: PARAMS_ERROR,
        desc: "error make <msg> — raise a structured error.",
    },
    // reflection
    Signature {
        name: "describe",
        input: Type::Any,
        output: Type::String,
        params: &[],
        desc: "Report the runtime type of the pipeline input.",
    },
    Signature {
        name: "length",
        input: Type::Any,
        output: Type::Int,
        params: &[],
        desc: "Item count for list/table; char count for string.",
    },
    Signature {
        name: "columns",
        input: Type::Record,
        output: Type::List,
        params: &[],
        desc: "List the keys of a record.",
    },
    Signature {
        name: "values",
        input: Type::Record,
        output: Type::List,
        params: &[],
        desc: "List the values of a record.",
    },
    Signature {
        name: "is-empty",
        input: Type::Any,
        output: Type::Bool,
        params: &[],
        desc: "True when the input is empty/Null.",
    },
    Signature {
        name: "help",
        input: Type::Any,
        output: Type::Any,
        params: PARAMS_HELP,
        desc: "Show the signature for a command, or list signed commands.",
    },
    // Phase 15a — full coverage of remaining value builtins
    Signature {
        name: "from-xml",
        input: Type::Stream,
        output: Type::Any,
        params: &[],
        desc: "Parse XML bytes into a record.",
    },
    Signature {
        name: "to-xml",
        input: Type::Any,
        output: Type::Stream,
        params: &[],
        desc: "Serialize a record to XML bytes.",
    },
    Signature {
        name: "ls",
        input: Type::Any,
        output: Type::Table,
        params: PARAMS_PATH_STR,
        desc: "List directory entries as a structured table.",
    },
    Signature {
        name: "ps",
        input: Type::Any,
        output: Type::Table,
        params: &[],
        desc: "List processes as a structured table.",
    },
    Signature {
        name: "open",
        input: Type::Any,
        output: Type::Any,
        params: PARAMS_REQ_PATH,
        desc: "Read a file; auto-decode by extension when known.",
    },
    Signature {
        name: "save",
        input: Type::Any,
        output: Type::Null,
        params: PARAMS_REQ_PATH,
        desc: "Write the pipeline input to a file.",
    },
    Signature {
        name: "update",
        input: Type::Any,
        output: Type::Any,
        params: PARAMS_FIELD_VAL,
        desc: "Update a field on each record (closure or literal).",
    },
    Signature {
        name: "insert",
        input: Type::Any,
        output: Type::Any,
        params: PARAMS_FIELD_VAL,
        desc: "Insert a new field on each record.",
    },
    Signature {
        name: "upsert",
        input: Type::Any,
        output: Type::Any,
        params: PARAMS_FIELD_VAL,
        desc: "Insert or replace a field on each record.",
    },
    Signature {
        name: "wrap",
        input: Type::Any,
        output: Type::Record,
        params: PARAMS_NAME,
        desc: "Wrap each value in a record with the given key.",
    },
    Signature {
        name: "flatten",
        input: Type::List,
        output: Type::List,
        params: &[],
        desc: "Flatten one level of nested lists.",
    },
    Signature {
        name: "into",
        input: Type::Any,
        output: Type::Any,
        params: PARAMS_INTO,
        desc: "Coerce input into the named type (int/string/...).",
    },
    Signature {
        name: "enumerate",
        input: Type::List,
        output: Type::Table,
        params: &[],
        desc: "Pair each item with its 0-based index.",
    },
    Signature {
        name: "zip",
        input: Type::List,
        output: Type::List,
        params: PARAMS_ZIP,
        desc: "Pair each item with the corresponding item of `other`.",
    },
    Signature {
        name: "move",
        input: Type::Table,
        output: Type::Table,
        params: PARAMS_MOVE,
        desc: "Reorder columns: move <col> --before/--after <pivot>.",
    },
    Signature {
        name: "merge",
        input: Type::Record,
        output: Type::Record,
        params: PARAMS_MERGE,
        desc: "Merge another record into the input record.",
    },
    Signature {
        name: "compact",
        input: Type::List,
        output: Type::List,
        params: &[],
        desc: "Drop Null/missing items from the list.",
    },
    Signature {
        name: "path",
        input: Type::Any,
        output: Type::Any,
        params: PARAMS_SUBOP,
        desc: "Path subcommand: join/dirname/basename/exists/...",
    },
    Signature {
        name: "date",
        input: Type::Any,
        output: Type::Any,
        params: PARAMS_SUBOP,
        desc: "Date subcommand: now/format/to-record/...",
    },
    Signature {
        name: "default",
        input: Type::Any,
        output: Type::Any,
        params: PARAMS_VALUE,
        desc: "Replace Null/missing with the given value.",
    },
    Signature {
        name: "transpose",
        input: Type::Table,
        output: Type::Table,
        params: &[],
        desc: "Swap rows and columns of a table.",
    },
    Signature {
        name: "shuffle",
        input: Type::List,
        output: Type::List,
        params: &[],
        desc: "Randomly reorder the list.",
    },
    Signature {
        name: "sort",
        input: Type::List,
        output: Type::List,
        params: &[],
        desc: "Sort a list of scalars.",
    },
    Signature {
        name: "unique",
        input: Type::List,
        output: Type::List,
        params: &[],
        desc: "Dedupe a list; -c also tallies counts.",
    },
    Signature {
        name: "count",
        input: Type::List,
        output: Type::Int,
        params: &[],
        desc: "Count items in a list (alias for `length`).",
    },
    Signature {
        name: "chunks",
        input: Type::List,
        output: Type::List,
        params: PARAMS_N,
        desc: "Split a list into chunks of size N.",
    },
    Signature {
        name: "window",
        input: Type::List,
        output: Type::List,
        params: PARAMS_N,
        desc: "Sliding window of size N over the list.",
    },
    Signature {
        name: "split-by",
        input: Type::List,
        output: Type::Record,
        params: PARAMS_SPLIT_BY,
        desc: "Partition a list by a key closure or field.",
    },
    Signature {
        name: "encode",
        input: Type::Any,
        output: Type::String,
        params: PARAMS_SCHEME,
        desc: "Encode bytes/string with base64 or hex.",
    },
    Signature {
        name: "decode",
        input: Type::String,
        output: Type::Any,
        params: PARAMS_SCHEME,
        desc: "Decode a string with base64 or hex.",
    },
    Signature {
        name: "url",
        input: Type::Any,
        output: Type::Any,
        params: PARAMS_SUBOP,
        desc: "URL subcommand: parse/join/encode/decode.",
    },
    Signature {
        name: "prepend",
        input: Type::List,
        output: Type::List,
        params: PARAMS_ITEMS,
        desc: "Prepend items to the front of a list.",
    },
    Signature {
        name: "append",
        input: Type::List,
        output: Type::List,
        params: PARAMS_ITEMS,
        desc: "Append items to the end of a list.",
    },
    Signature {
        name: "headers",
        input: Type::Table,
        output: Type::Table,
        params: &[],
        desc: "Promote the first row to column headers.",
    },
    Signature {
        name: "histogram",
        input: Type::List,
        output: Type::Table,
        params: PARAMS_PATH_STR,
        desc: "Count occurrences (or by a field name).",
    },
    Signature {
        name: "char",
        input: Type::Any,
        output: Type::String,
        params: PARAMS_CHAR,
        desc: "Look up a named character (nl/tab/sp/...).",
    },
    Signature {
        name: "ansi",
        input: Type::Any,
        output: Type::String,
        params: PARAMS_CHAR,
        desc: "Look up a named ANSI escape (red/reset/...).",
    },
    Signature {
        name: "fill",
        input: Type::Any,
        output: Type::String,
        params: &[],
        desc: "Pad input to width/character with flags.",
    },
    // Phase 16a — module import
    Signature {
        name: "use",
        input: Type::Null,
        output: Type::Null,
        params: PARAMS_USE,
        desc: "Import `def`s from a file: use PATH [name ...].",
    },
    // Phase 16c — http client
    Signature {
        name: "http",
        input: Type::Any,
        output: Type::Record,
        params: PARAMS_HTTP,
        desc: "HTTP client: http get|post|put|delete URL [-H K:V] [-d body] [--json].",
    },
];

const PARAMS_USE: &[Param] = &[p("path", Type::String), p_rest("names", Type::String)];
const PARAMS_HTTP: &[Param] = &[
    p("method", Type::String),
    p("url", Type::String),
    p_rest("opts", Type::String),
];

// ---------------------------------------------------------------------------
// Phase 15c — runtime signatures for user-defined functions (`def`)
// ---------------------------------------------------------------------------

#[derive(Debug, Clone)]
pub struct RuntimeParam {
    pub name: String,
    pub kind: Type,
    pub optional: bool,
    pub rest: bool,
}

#[derive(Debug, Clone)]
pub struct RuntimeSignature {
    pub name: String,
    pub params: Vec<RuntimeParam>,
    pub desc: String,
}

impl RuntimeSignature {
    /// Mirror Signature::validate_args for runtime sigs. Same flag/positional
    /// heuristic as static signatures.
    pub fn validate_args(&self, args: &[String]) -> Result<(), String> {
        let mut leading_positional = 0usize;
        let mut saw_flag = false;
        let mut total_positional = 0usize;
        for a in args {
            if a.starts_with('-') && a.len() > 1 && !a.chars().nth(1).unwrap().is_ascii_digit() {
                saw_flag = true;
            } else {
                total_positional += 1;
                if !saw_flag {
                    leading_positional += 1;
                }
            }
        }

        let required = self
            .params
            .iter()
            .take_while(|p| !p.optional && !p.rest)
            .count();
        let has_rest = self.params.iter().any(|p| p.rest);
        let max = if has_rest {
            usize::MAX
        } else {
            self.params.len()
        };

        if !saw_flag && leading_positional < required {
            let names: Vec<&str> = self
                .params
                .iter()
                .take(required)
                .map(|p| p.name.as_str())
                .collect();
            return Err(format!(
                "{}: missing required arg `{}` (expected {}: {})",
                self.name,
                names.get(leading_positional).copied().unwrap_or("?"),
                required,
                names.join(", "),
            ));
        }
        if !saw_flag && total_positional > max {
            return Err(format!(
                "{}: too many args (expected at most {}, got {})",
                self.name, max, total_positional,
            ));
        }
        Ok(())
    }

    /// Render help in the same shape as Signature::render_help.
    pub fn render_help(&self) -> String {
        let mut out = String::new();
        out.push_str(&format!("{} :: user-defined\n", self.name));
        if !self.desc.is_empty() {
            out.push_str(&format!("  {}\n", self.desc));
        }
        if !self.params.is_empty() {
            out.push_str("\nParameters:\n");
            for p in &self.params {
                let tag = if p.rest {
                    "..."
                } else if p.optional {
                    "?"
                } else {
                    ""
                };
                out.push_str(&format!("  {}{} : {}\n", tag, p.name, p.kind.render()));
            }
        }
        out
    }
}

/// Parse a single `def`-style param token, e.g. `a`, `a:int`, `a?:int`,
/// `cols...:string`. Returns None on malformed input.
pub fn parse_def_param(tok: &str) -> Option<RuntimeParam> {
    let (head, kind_str) = match tok.split_once(':') {
        Some((h, t)) => (h, Some(t)),
        None => (tok, None),
    };
    let (name, optional, rest) = if let Some(stripped) = head.strip_suffix("...") {
        (stripped.to_string(), false, true)
    } else if let Some(stripped) = head.strip_suffix('?') {
        (stripped.to_string(), true, false)
    } else {
        (head.to_string(), false, false)
    };
    if name.is_empty() {
        return None;
    }
    let kind = match kind_str.map(|s| s.trim()) {
        None | Some("any") => Type::Any,
        Some("int") => Type::Int,
        Some("float") => Type::Float,
        Some("number") => Type::Number,
        Some("string") => Type::String,
        Some("bool") => Type::Bool,
        Some("list") => Type::List,
        Some("record") => Type::Record,
        Some("table") => Type::Table,
        Some("closure") => Type::Closure,
        Some("path") => Type::Path,
        Some("null") | Some("nothing") => Type::Null,
        Some(_) => return None,
    };
    Some(RuntimeParam {
        name,
        kind,
        optional,
        rest,
    })
}

/// Light runtime type check for `def` arg coercion. Returns Err with a
/// human-readable message if the value clearly doesn't fit. `Any` always
/// passes. Number accepts int or float. Strict types reject mismatches.
pub fn check_value_type(v: &Value, kind: Type, param_name: &str) -> Result<(), String> {
    use Value as V;
    let ok = match kind {
        Type::Any => true,
        Type::Null => matches!(v, V::Null),
        Type::Bool => matches!(v, V::Bool(_)),
        Type::Int => matches!(v, V::Int(_)),
        Type::Float => matches!(v, V::Float(_) | V::Int(_)),
        Type::Number => matches!(v, V::Int(_) | V::Float(_)),
        Type::String => matches!(v, V::String(_)),
        Type::List => matches!(v, V::List(_)),
        Type::Record => matches!(v, V::Record(_)),
        Type::Table => matches!(v, V::List(_)), // table is list-of-records
        Type::Closure => matches!(v, V::Closure(_)),
        Type::Stream | Type::Path | Type::Binary => true, // not strictly checkable
        Type::Union(parts) => parts
            .iter()
            .any(|p| check_value_type(v, *p, param_name).is_ok()),
    };
    if ok {
        Ok(())
    } else {
        let got = match v {
            V::Null => "null",
            V::Bool(_) => "bool",
            V::Int(_) => "int",
            V::Float(_) => "float",
            V::String(_) => "string",
            V::List(_) => "list",
            V::Record(_) => "record",
            V::Closure(_) => "closure",
            V::Binary(_) => "binary",
        };
        Err(format!(
            "`{}`: expected {}, got {}",
            param_name,
            kind.render(),
            got
        ))
    }
}

pub static SIGNATURES: Lazy<HashMap<&'static str, &'static Signature>> = Lazy::new(|| {
    let mut m: HashMap<&'static str, &'static Signature> = HashMap::new();
    for s in SIGS {
        m.insert(s.name, s);
    }
    m
});

// ---------------------------------------------------------------------------
// Phase 16d — signature hints
// ---------------------------------------------------------------------------

/// Find the offset of the command segment that contains `cursor`.
/// Returns (segment_start, segment_end) where segment is delimited by
/// `|`, `;`, or newline. Cursor at exactly a delimiter is treated as being
/// in the segment that ENDS at that delimiter.
fn segment_around(buffer: &str, cursor: usize) -> (usize, usize) {
    let bytes = buffer.as_bytes();
    let cursor = cursor.min(buffer.len());
    let mut start = 0usize;
    for (i, &byte) in bytes.iter().enumerate().take(cursor) {
        let c = byte as char;
        if c == '|' || c == ';' || c == '\n' {
            start = i + 1;
        }
    }
    let mut end = buffer.len();
    for (i, &byte) in bytes.iter().enumerate().skip(cursor) {
        let c = byte as char;
        if c == '|' || c == ';' || c == '\n' {
            end = i;
            break;
        }
    }
    (start, end)
}

/// Tokenize a segment into (token_start, token_text) pairs by whitespace.
/// Ignores quoting subtleties — this is best-effort for hint display.
fn tokenize_segment(seg: &str, seg_start: usize) -> Vec<(usize, String)> {
    let mut out = Vec::new();
    let mut cur = String::new();
    let mut cur_start: Option<usize> = None;
    for (i, ch) in seg.char_indices() {
        if ch.is_whitespace() {
            if let Some(s) = cur_start.take() {
                out.push((s + seg_start, std::mem::take(&mut cur)));
            }
        } else {
            if cur_start.is_none() {
                cur_start = Some(i);
            }
            cur.push(ch);
        }
    }
    if let Some(s) = cur_start {
        out.push((s + seg_start, cur));
    }
    out
}

/// Render a one-line signature hint for the command segment containing `cursor`.
/// Returns None if no known command starts the segment.
///
/// Looks up `state.user_signatures` first (so user-defined `def`s shadow
/// builtins for the hint), then SIGNATURES.
pub fn hint_for(
    buffer: &str,
    cursor: usize,
    user_sigs: &std::collections::HashMap<String, RuntimeSignature>,
) -> Option<String> {
    if buffer.trim().is_empty() {
        return None;
    }
    let (seg_start, seg_end) = segment_around(buffer, cursor);
    let seg = &buffer[seg_start..seg_end];
    let toks = tokenize_segment(seg, seg_start);
    let (cmd_tok_start, cmd_name) = toks.first()?.clone();
    // Cursor before/at the command-name token start → don't hint yet.
    if cursor <= cmd_tok_start {
        return None;
    }

    // Active positional index: count non-flag tokens after the command,
    // up to (but not including) the cursor's token.
    // If the cursor is inside a token, that token is the active one;
    // if between tokens (whitespace), the NEXT positional is active.
    let positionals: Vec<&(usize, String)> = toks
        .iter()
        .skip(1)
        .filter(|(_, t)| {
            !(t.starts_with('-') && t.len() > 1 && !t.chars().nth(1).unwrap().is_ascii_digit())
        })
        .collect();

    let mut active = positionals.len(); // default: pointing past last
    for (i, (start, text)) in positionals.iter().enumerate() {
        let end = start + text.len();
        if cursor <= end {
            active = i;
            break;
        }
    }

    // Pull params from runtime sig first, then static.
    let (params, desc): (Vec<(String, String, bool, bool)>, String) =
        if let Some(rs) = user_sigs.get(&cmd_name) {
            (
                rs.params
                    .iter()
                    .map(|p| (p.name.clone(), p.kind.render(), p.optional, p.rest))
                    .collect(),
                rs.desc.clone(),
            )
        } else {
            let s = SIGNATURES.get(cmd_name.as_str())?;
            (
                s.params
                    .iter()
                    .map(|p| (p.name.to_string(), p.kind.render(), p.optional, p.rest))
                    .collect(),
                s.desc.to_string(),
            )
        };

    if params.is_empty() && desc.is_empty() {
        return None;
    }

    let mut parts: Vec<String> = Vec::with_capacity(params.len());
    for (i, (name, kind, opt, rest)) in params.iter().enumerate() {
        let tag = if *rest {
            "..."
        } else if *opt {
            "?"
        } else {
            ""
        };
        let is_active = if *rest { i <= active } else { i == active };
        let piece = format!("{}{}:{}", tag, name, kind);
        if is_active {
            // ANSI: bold + cyan; reset before continuing
            parts.push(format!("\x1b[1;36m{}\x1b[0;2m", piece));
        } else {
            parts.push(piece);
        }
    }
    let params_str = if parts.is_empty() {
        String::new()
    } else {
        parts.join(" ")
    };
    let mut line = format!("\x1b[2m{} {}", cmd_name, params_str);
    if !desc.is_empty() {
        if !params_str.is_empty() {
            line.push_str("  ");
        }
        line.push_str("— ");
        line.push_str(&desc);
    }
    line.push_str("\x1b[0m");
    Some(line)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn signature_has_input_output() {
        let s = SIGNATURES.get("where").expect("where signature exists");
        assert_eq!(s.input, Type::List);
        assert_eq!(s.output, Type::List);
        assert_eq!(s.params.len(), 1);
    }

    #[test]
    fn render_help_includes_name_and_desc() {
        let s = SIGNATURES.get("from-json").unwrap();
        let h = s.render_help();
        assert!(h.contains("from-json"));
        assert!(h.contains("Parse JSON"));
    }

    #[test]
    fn to_record_yields_record() {
        let s = SIGNATURES.get("each").unwrap();
        let v = s.to_record();
        match v {
            Value::Record(m) => {
                assert_eq!(
                    m.get("name").and_then(|v| match v {
                        Value::String(s) => Some(s.as_str()),
                        _ => None,
                    }),
                    Some("each"),
                );
            }
            _ => panic!("expected Record"),
        }
    }

    #[test]
    fn coverage_spot_check() {
        // The registry should cover at least the core commands.
        for n in &[
            "from-json",
            "to-json",
            "where",
            "each",
            "select",
            "math",
            "try",
            "error",
            "help",
        ] {
            assert!(SIGNATURES.contains_key(*n), "missing signature: {}", n);
        }
    }

    #[test]
    fn hint_for_known_builtin_renders_name_and_params() {
        let empty: std::collections::HashMap<String, RuntimeSignature> = Default::default();
        let h = hint_for("each ", 5, &empty).expect("each is a known signature");
        assert!(h.contains("each"), "got: {}", h);
        assert!(h.contains("closure"), "got: {}", h);
    }

    #[test]
    fn hint_for_unknown_command_returns_none() {
        let empty: std::collections::HashMap<String, RuntimeSignature> = Default::default();
        assert!(hint_for("xyznotacommand foo", 17, &empty).is_none());
    }

    #[test]
    fn hint_for_picks_segment_after_pipe() {
        let empty: std::collections::HashMap<String, RuntimeSignature> = Default::default();
        // Cursor is in the `where` segment; hint should describe `where`.
        let src = "ls | where size > 0";
        let cur = src.find("where").unwrap() + 5;
        let h = hint_for(src, cur, &empty).expect("where is known");
        assert!(h.contains("where"), "got: {}", h);
    }

    #[test]
    fn hint_for_user_def_overrides_builtin() {
        let mut sigs = std::collections::HashMap::new();
        sigs.insert(
            "each".to_string(),
            RuntimeSignature {
                name: "each".to_string(),
                params: vec![RuntimeParam {
                    name: "x".into(),
                    kind: Type::Int,
                    optional: false,
                    rest: false,
                }],
                desc: "USER".into(),
            },
        );
        let h = hint_for("each 1", 6, &sigs).expect("user each is known");
        assert!(h.contains("USER"), "got: {}", h);
    }

    #[test]
    fn hint_for_empty_buffer_is_none() {
        let empty: std::collections::HashMap<String, RuntimeSignature> = Default::default();
        assert!(hint_for("", 0, &empty).is_none());
        assert!(hint_for("    ", 2, &empty).is_none());
    }
}
