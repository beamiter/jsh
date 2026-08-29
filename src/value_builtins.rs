/// Value-aware (in-process) builtins.
///
/// Each fn takes `PipelineData` input and returns `PipelineData`, threading
/// typed `Value`s through the pipeline without forking.
use crate::environment::ShellState;
use crate::pipeline_data::PipelineData;
use crate::value::{render_table, ClosureData, Value};
use indexmap::IndexMap;
use once_cell::sync::Lazy;
use std::collections::HashMap;
use std::sync::Arc;

const MAX_OPEN_FILE_BYTES: usize = 64 * 1024 * 1024;
const MAX_MODULE_FILE_BYTES: usize = 16 * 1024 * 1024;
#[cfg(feature = "ai")]
const MAX_HTTP_BODY_BYTES: usize = 16 * 1024 * 1024;
#[cfg(feature = "ai")]
const MAX_HTTP_URL_BYTES: usize = 16 * 1024;
#[cfg(feature = "ai")]
const MAX_HTTP_HEADERS: usize = 128;
#[cfg(feature = "ai")]
const MAX_HTTP_HEADER_BYTES: usize = 64 * 1024;

pub type ValueBuiltin = fn(PipelineData, &[String], &mut ShellState) -> Result<PipelineData, i32>;

pub static VALUE_BUILTINS: Lazy<HashMap<&'static str, ValueBuiltin>> = Lazy::new(|| {
    let mut m: HashMap<&'static str, ValueBuiltin> = HashMap::new();
    m.insert("from-json", vb_from_json);
    m.insert("from-ndjson", vb_from_ndjson);
    m.insert("to-json", vb_to_json);
    m.insert("to-table", vb_to_table);
    m.insert("where", vb_where);
    m.insert("sort-by", vb_sort_by);
    m.insert("select", vb_select);
    m.insert("group-by", vb_group_by);
    m.insert("unique", vb_unique);
    m.insert("count", vb_count);
    m.insert("math", vb_math);
    m.insert("from-csv", vb_from_csv);
    m.insert("length", vb_length);
    m.insert("first", vb_first);
    m.insert("last", vb_last);
    m.insert("reverse", vb_reverse);
    m.insert("each", vb_each);
    // Phase 16b — parallel each
    m.insert("par-each", vb_par_each);
    m.insert("from-yaml", vb_from_yaml);
    m.insert("to-yaml", vb_to_yaml);
    m.insert("from-toml", vb_from_toml);
    m.insert("to-toml", vb_to_toml);
    m.insert("from-xml", vb_from_xml);
    m.insert("to-xml", vb_to_xml);
    m.insert("ls", vb_ls);
    m.insert("ps", vb_ps);
    m.insert("open", vb_open);
    m.insert("save", vb_save);
    m.insert("lines", vb_lines);
    m.insert("split", vb_split);
    m.insert("parse", vb_parse);
    m.insert("str", vb_str);
    m.insert("get", vb_get);
    m.insert("update", vb_update);
    m.insert("insert", vb_insert);
    m.insert("reject", vb_reject);
    m.insert("wrap", vb_wrap);
    m.insert("flatten", vb_flatten);
    m.insert("into", vb_into);
    m.insert("range", vb_range);
    // Phase 7a — iteration combinators
    m.insert("reduce", vb_reduce);
    m.insert("take", vb_take);
    m.insert("skip", vb_skip);
    m.insert("enumerate", vb_enumerate);
    m.insert("zip", vb_zip);
    // Phase 7b — record/table shaping
    m.insert("columns", vb_columns);
    m.insert("values", vb_values);
    m.insert("rename", vb_rename);
    m.insert("move", vb_move);
    m.insert("merge", vb_merge);
    m.insert("upsert", vb_upsert);
    m.insert("compact", vb_compact);
    // Phase 7c — predicates / reflection
    m.insert("any", vb_any);
    m.insert("all", vb_all);
    m.insert("is-empty", vb_is_empty);
    m.insert("describe", vb_describe);
    // Phase 7d — path / date
    m.insert("path", vb_path);
    m.insert("date", vb_date);
    // Phase 9b — format template
    m.insert("format", vb_format);
    // Phase 9c — do (execute closure inline)
    m.insert("do", vb_do);
    // Phase 10c — table utilities
    m.insert("default", vb_default);
    m.insert("transpose", vb_transpose);
    m.insert("shuffle", vb_shuffle);
    // Phase 11a — more data utilities
    m.insert("sort", vb_sort);
    m.insert("to-csv", vb_to_csv);
    m.insert("chunks", vb_chunks);
    m.insert("window", vb_window);
    m.insert("split-by", vb_split_by);
    // Phase 11b — encoding
    m.insert("encode", vb_encode);
    m.insert("decode", vb_decode);
    // Phase 12c — url parse/join
    m.insert("url", vb_url);
    // Phase 13a — list utilities
    m.insert("prepend", vb_prepend);
    m.insert("append", vb_append);
    m.insert("drop", vb_drop);
    m.insert("headers", vb_headers);
    // Phase 13b — analytics + string distance
    m.insert("histogram", vb_histogram);
    // Phase 13c — char / ansi / fill
    m.insert("char", vb_char);
    m.insert("ansi", vb_ansi);
    m.insert("fill", vb_fill);
    // Phase 14a — structured errors + try/catch
    m.insert("try", vb_try);
    m.insert("error", vb_error);
    // Phase 14b — signatures / help
    m.insert("help", vb_help);
    // Phase 15c — typed user functions
    m.insert("def", vb_def);
    // Phase 16a — module import
    m.insert("use", vb_use);
    // Phase 16c — http client
    m.insert("http", vb_http);
    m
});

/// Builtins that are *always* treated as value-aware (override bash/external).
/// `ls`/`ps` are intentionally excluded so a bare `ls` still runs the system
/// command — they're only value-aware inside multi-command pipelines (see
/// `is_value_aware_in_pipeline`).
pub fn is_value_aware(name: &str) -> bool {
    VALUE_BUILTINS.contains_key(name) && !is_context_only(name)
}

/// Used by the pipeline pre-flight check: ls/ps participate as value-aware
/// only when there's at least one other command in the pipeline (e.g.
/// `ls | where ...`). For a bare `ls`, dispatch falls through to external.
pub fn is_value_aware_in_pipeline(name: &str) -> bool {
    VALUE_BUILTINS.contains_key(name)
}

fn is_context_only(name: &str) -> bool {
    matches!(name, "ls" | "ps")
}

// ---------------------------------------------------------------------------
// Individual builtins
// ---------------------------------------------------------------------------

fn vb_from_json(
    input: PipelineData,
    _args: &[String],
    _state: &mut ShellState,
) -> Result<PipelineData, i32> {
    let vs = input.into_values()?;
    Ok(PipelineData::Values(vs))
}

/// Phase 15b — streaming NDJSON parser. Parses one JSON value per line
/// lazily, so `from-ndjson big.jsonl | take 5 | ...` doesn't need to
/// touch every line of the file.
fn vb_from_ndjson(
    input: PipelineData,
    _args: &[String],
    _state: &mut ShellState,
) -> Result<PipelineData, i32> {
    let bytes: Vec<u8> = match input {
        PipelineData::Empty => Vec::new(),
        PipelineData::Bytes(b) => b,
        PipelineData::Values(vs) => vs
            .iter()
            .map(|v| v.to_display_string())
            .collect::<Vec<_>>()
            .join("\n")
            .into_bytes(),
        PipelineData::Stream(it) => it
            .map(|v| v.to_display_string())
            .collect::<Vec<_>>()
            .join("\n")
            .into_bytes(),
    };
    let s = String::from_utf8_lossy(&bytes).into_owned();
    // Owned String → owned iterator (deferred parse, line-by-line).
    let it = s.into_bytes();
    let s = unsafe { String::from_utf8_unchecked(it) };
    // Split now (cheap) and parse lazily.
    let lines: Vec<String> = s
        .lines()
        .filter(|l| !l.trim().is_empty())
        .map(|l| l.to_string())
        .collect();
    let iter = lines.into_iter().filter_map(|line| {
        serde_json::from_str::<serde_json::Value>(&line)
            .ok()
            .map(Value::from_json)
    });
    Ok(PipelineData::Stream(Box::new(iter)))
}

fn vb_to_json(
    input: PipelineData,
    _args: &[String],
    _state: &mut ShellState,
) -> Result<PipelineData, i32> {
    let vs = input.into_values()?;
    let bytes = PipelineData::Values(vs).into_bytes();
    Ok(PipelineData::Bytes(bytes))
}

fn vb_to_table(
    input: PipelineData,
    _args: &[String],
    _state: &mut ShellState,
) -> Result<PipelineData, i32> {
    let vs = input.into_values()?;
    Ok(PipelineData::Bytes(render_table(&vs).into_bytes()))
}

fn vb_where(
    input: PipelineData,
    args: &[String],
    state: &mut ShellState,
) -> Result<PipelineData, i32> {
    // Closure form: `where {|r| ...}` or `where $f` where f is a Value::Closure.
    if let Some(closure) = lookup_closure(args.first(), state) {
        let vs = input.into_values()?;
        let mut out = Vec::with_capacity(vs.len());
        for v in vs {
            let result = crate::executor::apply_closure(&closure, std::slice::from_ref(&v), state)?;
            if is_truthy(&result) {
                out.push(v);
            }
        }
        return Ok(PipelineData::Values(out));
    }
    if args.len() < 3 {
        eprintln!("Usage: where <field> <op> <value> | where {{|r| ...}}");
        return Err(1);
    }
    let field = &args[0];
    let op = &args[1];
    let rhs = &args[2];
    let vs = input.into_values()?;
    let out: Vec<Value> = vs
        .into_iter()
        .filter(|v| v.get(field).map(|fv| compare(fv, op, rhs)).unwrap_or(false))
        .collect();
    Ok(PipelineData::Values(out))
}

fn vb_each(
    input: PipelineData,
    args: &[String],
    state: &mut ShellState,
) -> Result<PipelineData, i32> {
    // Split flags out from the closure argument: `each [-k|--keep-empty] {|x| ...}`.
    // Without -k, closure results that are Null are dropped (matches nushell's
    // default of treating Null as "skip this row").
    let mut keep_empty = false;
    let mut closure_arg: Option<&String> = None;
    for a in args {
        match a.as_str() {
            "-k" | "--keep-empty" => keep_empty = true,
            _ => {
                if closure_arg.is_none() {
                    closure_arg = Some(a);
                }
            }
        }
    }
    let closure = match lookup_closure(closure_arg, state) {
        Some(c) => c,
        None => {
            eprintln!("Usage: each [-k|--keep-empty] {{|x| ...}}");
            return Err(1);
        }
    };
    let vs = input.into_values()?;
    let mut out = Vec::with_capacity(vs.len());
    for v in vs {
        let r = crate::executor::apply_closure(&closure, std::slice::from_ref(&v), state)?;
        if !keep_empty && matches!(r, Value::Null) {
            continue;
        }
        out.push(r);
    }
    Ok(PipelineData::Values(out))
}

/// Phase 16b — parallel `each`. Runs the closure body on a worker pool while
/// preserving input order. Restricted to *pure-expression* closures (the same
/// fast path used by closure_expr::try_eval) so the workers never touch
/// ShellState. Non-pure bodies fall back to sequential `each` for correctness.
fn vb_par_each(
    input: PipelineData,
    args: &[String],
    state: &mut ShellState,
) -> Result<PipelineData, i32> {
    use std::sync::{Arc, Mutex};
    let mut keep_empty = false;
    let mut closure_arg: Option<&String> = None;
    let mut threads_override: Option<usize> = None;
    let mut i = 0;
    while i < args.len() {
        match args[i].as_str() {
            "-k" | "--keep-empty" => keep_empty = true,
            "-t" | "--threads" => {
                threads_override = args.get(i + 1).and_then(|s| s.parse().ok());
                i += 1;
            }
            _ => {
                if closure_arg.is_none() {
                    closure_arg = Some(&args[i]);
                }
            }
        }
        i += 1;
    }

    let closure = match lookup_closure(closure_arg, state) {
        Some(c) => c,
        None => {
            eprintln!("Usage: par-each [-k] [-t N] {{|x| ...}}");
            return Err(1);
        }
    };
    // Pre-flight: if the body isn't a pure expression, fall back to sequential
    // each so semantics (closures that call builtins, mutate state, etc.) are
    // preserved. We probe with an empty var map — try_eval doesn't need the
    // real params to decide if the body is parseable as an expression.
    let probe_vars: std::collections::HashMap<String, Value> = closure.captured.clone();
    let is_pure = matches!(
        crate::closure_expr::try_eval(&closure.body_src, &probe_vars),
        Ok(Some(_)) | Err(_)
    );
    if !is_pure {
        // Fall back: defer to vb_each by re-dispatching with the same args.
        return vb_each(input, args, state);
    }

    let vs = input.into_values()?;
    let n = vs.len();
    if n == 0 {
        return Ok(PipelineData::Values(Vec::new()));
    }

    let workers = threads_override
        .unwrap_or_else(|| {
            std::thread::available_parallelism()
                .map(|p| p.get())
                .unwrap_or(4)
        })
        .max(1)
        .min(n);

    let param = closure
        .params
        .first()
        .cloned()
        .unwrap_or_else(|| "in".to_string());
    let body = Arc::new(closure.body_src.clone());
    let captured = Arc::new(closure.captured.clone());
    let param = Arc::new(param);
    let results = Arc::new(Mutex::new(vec![Value::Null; n]));
    let err_slot: Arc<Mutex<Option<String>>> = Arc::new(Mutex::new(None));

    // Static chunking — simpler than a work-stealing queue and good enough for
    // the typical "expensive but uniform" workload.
    let chunk = n.div_ceil(workers);
    let work: Vec<(usize, Value)> = vs.into_iter().enumerate().collect();

    let mut handles = Vec::with_capacity(workers);
    for w in 0..workers {
        let start = w * chunk;
        let end = (start + chunk).min(n);
        if start >= end {
            break;
        }
        let slice = work[start..end].to_vec();
        let results = results.clone();
        let err_slot = err_slot.clone();
        let spawn_error_slot = err_slot.clone();
        let body = body.clone();
        let captured = captured.clone();
        let param = param.clone();
        let worker = std::thread::Builder::new()
            .name(format!("jsh-par-each-{w}"))
            .spawn(move || {
                for (idx, val) in slice {
                    if err_slot.lock().unwrap().is_some() {
                        return;
                    }
                    let mut vars: std::collections::HashMap<String, Value> = (*captured).clone();
                    vars.insert((*param).clone(), val.clone());
                    vars.insert("in".to_string(), val);
                    match crate::closure_expr::try_eval(&body, &vars) {
                        Ok(Some(v)) => {
                            results.lock().unwrap()[idx] = v;
                        }
                        Ok(None) => {
                            *err_slot.lock().unwrap() =
                                Some("par-each: closure body is not a pure expression".to_string());
                            return;
                        }
                        Err(msg) => {
                            *err_slot.lock().unwrap() = Some(msg);
                            return;
                        }
                    }
                }
            });
        match worker {
            Ok(handle) => handles.push(handle),
            Err(error) => {
                *spawn_error_slot.lock().unwrap() =
                    Some(format!("par-each: could not start worker thread: {error}"));
                break;
            }
        }
    }
    for h in handles {
        let _ = h.join();
    }

    if let Some(msg) = err_slot.lock().unwrap().take() {
        eprintln!("jsh: {}", msg);
        state.set_error(msg, 1);
        return Err(1);
    }
    let out_vec = Arc::try_unwrap(results)
        .map_err(|_| 1_i32)?
        .into_inner()
        .map_err(|_| 1_i32)?;
    let final_out: Vec<Value> = if keep_empty {
        out_vec
    } else {
        out_vec
            .into_iter()
            .filter(|v| !matches!(v, Value::Null))
            .collect()
    };
    Ok(PipelineData::Values(final_out))
}

fn vb_sort_by(
    input: PipelineData,
    args: &[String],
    _state: &mut ShellState,
) -> Result<PipelineData, i32> {
    if args.is_empty() {
        eprintln!("Usage: sort-by <field> [-r]");
        return Err(1);
    }
    let field = &args[0];
    let reverse = args.get(1).map(|s| s == "-r").unwrap_or(false);
    let mut vs = input.into_values()?;
    vs.sort_by(|a, b| {
        let va = a.get(field);
        let vb = b.get(field);
        let cmp = compare_opt(va, vb);
        if reverse {
            cmp.reverse()
        } else {
            cmp
        }
    });
    Ok(PipelineData::Values(vs))
}

fn vb_select(
    input: PipelineData,
    args: &[String],
    _state: &mut ShellState,
) -> Result<PipelineData, i32> {
    if args.is_empty() {
        eprintln!("Usage: select <field1> [field2] ...");
        return Err(1);
    }
    let vs = input.into_values()?;
    let out: Vec<Value> = vs
        .into_iter()
        .map(|v| match v {
            Value::Record(map) => {
                let mut new_map = IndexMap::new();
                for f in args {
                    if let Some(val) = map.get(f) {
                        new_map.insert(f.clone(), val.clone());
                    }
                }
                Value::Record(new_map)
            }
            other => other,
        })
        .collect();
    Ok(PipelineData::Values(out))
}

fn vb_group_by(
    input: PipelineData,
    args: &[String],
    _state: &mut ShellState,
) -> Result<PipelineData, i32> {
    if args.is_empty() {
        eprintln!("Usage: group-by <field>");
        return Err(1);
    }
    let field = &args[0];
    let vs = input.into_values()?;
    let mut groups: IndexMap<String, Vec<Value>> = IndexMap::new();
    for v in vs {
        let key = v
            .get(field)
            .map(|fv| fv.to_display_string())
            .unwrap_or_else(|| "null".to_string());
        groups.entry(key).or_default().push(v);
    }
    let mut rec = IndexMap::new();
    for (k, items) in groups {
        rec.insert(k, Value::List(items));
    }
    Ok(PipelineData::Values(vec![Value::Record(rec)]))
}

fn vb_unique(
    input: PipelineData,
    args: &[String],
    _state: &mut ShellState,
) -> Result<PipelineData, i32> {
    // `unique [-c|--count] [field]`. With -c the output is a list of
    // {value, count} records sorted by descending count.
    let mut count_mode = false;
    let mut field: Option<&str> = None;
    for a in args {
        match a.as_str() {
            "-c" | "--count" => count_mode = true,
            _ => field = Some(a.as_str()),
        }
    }
    let vs = input.into_values()?;
    if count_mode {
        let mut order: Vec<String> = Vec::new();
        let mut counts: std::collections::HashMap<String, (Value, i64)> =
            std::collections::HashMap::new();
        for v in vs {
            let key = match field {
                Some(f) => v
                    .get(f)
                    .map(|fv| fv.to_display_string())
                    .unwrap_or_default(),
                None => v.to_display_string(),
            };
            let entry = counts.entry(key.clone()).or_insert_with(|| {
                order.push(key.clone());
                (v.clone(), 0)
            });
            entry.1 += 1;
        }
        let mut rows: Vec<Value> = order
            .into_iter()
            .map(|k| {
                let (val, n) = counts.remove(&k).unwrap();
                let mut rec = IndexMap::new();
                rec.insert("value".to_string(), val);
                rec.insert("count".to_string(), Value::Int(n));
                Value::Record(rec)
            })
            .collect();
        rows.sort_by(|a, b| {
            let ac = a.get("count").and_then(|v| v.as_f64()).unwrap_or(0.0);
            let bc = b.get("count").and_then(|v| v.as_f64()).unwrap_or(0.0);
            bc.partial_cmp(&ac).unwrap_or(std::cmp::Ordering::Equal)
        });
        return Ok(PipelineData::Values(rows));
    }
    let mut seen = std::collections::HashSet::new();
    let mut out = Vec::new();
    for v in vs {
        let key = match field {
            Some(f) => v
                .get(f)
                .map(|fv| fv.to_display_string())
                .unwrap_or_default(),
            None => v.to_display_string(),
        };
        if seen.insert(key) {
            out.push(v);
        }
    }
    Ok(PipelineData::Values(out))
}

fn vb_count(
    input: PipelineData,
    _args: &[String],
    _state: &mut ShellState,
) -> Result<PipelineData, i32> {
    let vs = input.into_values()?;
    Ok(PipelineData::Values(vec![Value::Int(vs.len() as i64)]))
}

fn vb_math(
    input: PipelineData,
    args: &[String],
    _state: &mut ShellState,
) -> Result<PipelineData, i32> {
    if args.is_empty() {
        eprintln!("Usage: math <op> [field]  (aggregations: sum|avg|min|max|stddev|median|product;  per-element: abs|ceil|floor|round|sqrt|log|log2|log10)");
        return Err(1);
    }
    let op = args[0].as_str();
    let field = args.get(1);

    // Per-element scalar operations: map over the input rather than aggregate.
    let elementwise = matches!(
        op,
        "abs" | "ceil" | "floor" | "round" | "sqrt" | "log" | "log2" | "log10"
    );
    if elementwise {
        let apply = |x: f64| -> f64 {
            match op {
                "abs" => x.abs(),
                "ceil" => x.ceil(),
                "floor" => x.floor(),
                "round" => x.round(),
                "sqrt" => x.sqrt(),
                "log" => x.ln(),
                "log2" => x.log2(),
                "log10" => x.log10(),
                _ => x,
            }
        };
        let to_value = |x: f64| -> Value {
            if x.fract() == 0.0 && x.is_finite() && x.abs() < 1e16 {
                Value::Int(x as i64)
            } else {
                Value::Float(x)
            }
        };
        let vs = input.into_values()?;
        let mut out = Vec::with_capacity(vs.len());
        for v in vs {
            if let Some(x) = v.as_f64() {
                out.push(to_value(apply(x)));
            } else {
                out.push(v);
            }
        }
        return Ok(PipelineData::Values(out));
    }

    let vs = input.into_values()?;
    // Aggregations: `math sum field` (extract field from records) OR
    // `math sum` (pipeline is a list of numbers).
    let nums: Vec<f64> = if let Some(f) = field {
        vs.iter()
            .filter_map(|v| v.get(f).and_then(|fv| fv.as_f64()))
            .collect()
    } else {
        vs.iter().filter_map(|v| v.as_f64()).collect()
    };
    if nums.is_empty() {
        eprintln!("math: no numeric values");
        return Err(1);
    }
    let r = match op {
        "sum" => nums.iter().sum::<f64>(),
        "avg" | "mean" => nums.iter().sum::<f64>() / nums.len() as f64,
        "min" => nums.iter().copied().fold(f64::INFINITY, f64::min),
        "max" => nums.iter().copied().fold(f64::NEG_INFINITY, f64::max),
        "product" => nums.iter().product::<f64>(),
        "median" => {
            let mut sorted = nums.clone();
            sorted.sort_by(|a, b| a.partial_cmp(b).unwrap_or(std::cmp::Ordering::Equal));
            let n = sorted.len();
            if n.is_multiple_of(2) {
                (sorted[n / 2 - 1] + sorted[n / 2]) / 2.0
            } else {
                sorted[n / 2]
            }
        }
        "stddev" | "std" => {
            let mean = nums.iter().sum::<f64>() / nums.len() as f64;
            let var = nums.iter().map(|x| (x - mean).powi(2)).sum::<f64>() / nums.len() as f64;
            var.sqrt()
        }
        "variance" | "var" => {
            let mean = nums.iter().sum::<f64>() / nums.len() as f64;
            nums.iter().map(|x| (x - mean).powi(2)).sum::<f64>() / nums.len() as f64
        }
        _ => {
            eprintln!("math: unknown op '{}'", op);
            return Err(1);
        }
    };
    let out = if r.fract() == 0.0 && r.is_finite() && r.abs() < 1e16 {
        Value::Int(r as i64)
    } else {
        Value::Float(r)
    };
    Ok(PipelineData::Values(vec![out]))
}

fn vb_from_csv(
    input: PipelineData,
    _args: &[String],
    _state: &mut ShellState,
) -> Result<PipelineData, i32> {
    let bytes = match input {
        PipelineData::Empty => Vec::new(),
        PipelineData::Bytes(b) => b,
        PipelineData::Values(_) | PipelineData::Stream(_) => {
            eprintln!("from-csv: expected text input");
            return Err(1);
        }
    };
    let s = String::from_utf8_lossy(&bytes);
    let lines: Vec<&str> = s.lines().collect();
    if lines.is_empty() {
        return Ok(PipelineData::Values(Vec::new()));
    }
    let headers = parse_csv_line(lines[0]);
    let mut out = Vec::new();
    for line in &lines[1..] {
        if line.trim().is_empty() {
            continue;
        }
        let cells = parse_csv_line(line);
        let mut rec = IndexMap::new();
        for (i, h) in headers.iter().enumerate() {
            let cell = cells.get(i).cloned().unwrap_or_default();
            // Try numeric coercion
            let val = if let Ok(i) = cell.parse::<i64>() {
                Value::Int(i)
            } else if let Ok(f) = cell.parse::<f64>() {
                Value::Float(f)
            } else {
                Value::String(cell)
            };
            rec.insert(h.clone(), val);
        }
        out.push(Value::Record(rec));
    }
    Ok(PipelineData::Values(out))
}

fn vb_length(
    input: PipelineData,
    _args: &[String],
    _state: &mut ShellState,
) -> Result<PipelineData, i32> {
    let vs = input.into_values()?;
    Ok(PipelineData::Values(vec![Value::Int(vs.len() as i64)]))
}

fn vb_first(
    input: PipelineData,
    args: &[String],
    _state: &mut ShellState,
) -> Result<PipelineData, i32> {
    let n: usize = args.first().and_then(|s| s.parse().ok()).unwrap_or(1);
    if input.is_stream() {
        let vs: Vec<Value> = input.into_value_iter()?.take(n).collect();
        return Ok(PipelineData::Values(vs));
    }
    let mut vs = input.into_values()?;
    vs.truncate(n);
    Ok(PipelineData::Values(vs))
}

fn vb_last(
    input: PipelineData,
    args: &[String],
    _state: &mut ShellState,
) -> Result<PipelineData, i32> {
    let n: usize = args.first().and_then(|s| s.parse().ok()).unwrap_or(1);
    let vs = input.into_values()?;
    let start = vs.len().saturating_sub(n);
    Ok(PipelineData::Values(vs[start..].to_vec()))
}

fn vb_reverse(
    input: PipelineData,
    _args: &[String],
    _state: &mut ShellState,
) -> Result<PipelineData, i32> {
    let mut vs = input.into_values()?;
    vs.reverse();
    Ok(PipelineData::Values(vs))
}

// ---------------------------------------------------------------------------
// Phase 5d — format converters
// ---------------------------------------------------------------------------

fn vb_from_yaml(
    input: PipelineData,
    _args: &[String],
    _state: &mut ShellState,
) -> Result<PipelineData, i32> {
    let bytes = match input {
        PipelineData::Empty => Vec::new(),
        PipelineData::Bytes(b) => b,
        PipelineData::Values(_) | PipelineData::Stream(_) => {
            eprintln!("from-yaml: expected text input");
            return Err(1);
        }
    };
    // serde_yaml_ng deserializes into serde_json::Value first (lossy-typed but JSON-compatible).
    let yval: serde_yaml_ng::Value = serde_yaml_ng::from_slice(&bytes).map_err(|e| {
        eprintln!("from-yaml: {}", e);
        1
    })?;
    let jval = yaml_to_json(yval);
    let v = Value::from_json(jval);
    let out = match v {
        Value::List(items) => items,
        other => vec![other],
    };
    Ok(PipelineData::Values(out))
}

fn vb_to_yaml(
    input: PipelineData,
    _args: &[String],
    _state: &mut ShellState,
) -> Result<PipelineData, i32> {
    let vs = input.into_values()?;
    let jval = if vs.len() == 1 {
        vs.into_iter().next().unwrap().to_json()
    } else {
        Value::List(vs).to_json()
    };
    let s = serde_yaml_ng::to_string(&jval).map_err(|e| {
        eprintln!("to-yaml: {}", e);
        1
    })?;
    Ok(PipelineData::Bytes(s.into_bytes()))
}

fn vb_from_toml(
    input: PipelineData,
    _args: &[String],
    _state: &mut ShellState,
) -> Result<PipelineData, i32> {
    let bytes = match input {
        PipelineData::Empty => Vec::new(),
        PipelineData::Bytes(b) => b,
        PipelineData::Values(_) | PipelineData::Stream(_) => {
            eprintln!("from-toml: expected text input");
            return Err(1);
        }
    };
    let s = String::from_utf8_lossy(&bytes);
    let tval: toml::Value = toml::from_str(&s).map_err(|e| {
        eprintln!("from-toml: {}", e);
        1
    })?;
    let v = Value::from_json(toml_to_json(tval));
    Ok(PipelineData::Values(vec![v]))
}

fn vb_to_toml(
    input: PipelineData,
    _args: &[String],
    _state: &mut ShellState,
) -> Result<PipelineData, i32> {
    let vs = input.into_values()?;
    if vs.len() != 1 || !vs[0].is_record() {
        eprintln!("to-toml: expects a single record");
        return Err(1);
    }
    let jval = vs.into_iter().next().unwrap().to_json();
    let s = toml::to_string(&jval).map_err(|e| {
        eprintln!("to-toml: {}", e);
        1
    })?;
    Ok(PipelineData::Bytes(s.into_bytes()))
}

fn vb_from_xml(
    input: PipelineData,
    _args: &[String],
    _state: &mut ShellState,
) -> Result<PipelineData, i32> {
    use quick_xml::events::Event;
    use quick_xml::reader::Reader;

    let bytes = match input {
        PipelineData::Empty => Vec::new(),
        PipelineData::Bytes(b) => b,
        PipelineData::Values(_) | PipelineData::Stream(_) => {
            eprintln!("from-xml: expected text input");
            return Err(1);
        }
    };
    let mut reader = Reader::from_reader(bytes.as_slice());
    reader.config_mut().trim_text(true);
    // Build a nested record: each element is {tag, attrs, content, children}.
    // Stack-based: top of stack is the open element we're populating.
    struct Open {
        tag: String,
        attrs: IndexMap<String, Value>,
        children: Vec<Value>,
        text: String,
    }
    let mut stack: Vec<Open> = Vec::new();
    let mut top_level: Vec<Value> = Vec::new();
    let mut buf = Vec::new();
    loop {
        match reader.read_event_into(&mut buf) {
            Ok(ev @ Event::Start(_)) | Ok(ev @ Event::Empty(_)) => {
                let is_empty = matches!(ev, Event::Empty(_));
                let e = match &ev {
                    Event::Start(e) | Event::Empty(e) => e,
                    _ => unreachable!(),
                };
                let tag = String::from_utf8_lossy(e.name().as_ref()).to_string();
                let mut attrs = IndexMap::new();
                for a in e.attributes().flatten() {
                    let k = String::from_utf8_lossy(a.key.as_ref()).to_string();
                    let v = String::from_utf8_lossy(&a.value).to_string();
                    attrs.insert(k, Value::String(v));
                }
                let opened = Open {
                    tag,
                    attrs,
                    children: Vec::new(),
                    text: String::new(),
                };
                stack.push(opened);
                if is_empty {
                    // Synthesize an End event by popping immediately.
                    if let Some(o) = stack.pop() {
                        let mut rec = IndexMap::new();
                        rec.insert("tag".to_string(), Value::String(o.tag));
                        if !o.attrs.is_empty() {
                            rec.insert("attrs".to_string(), Value::Record(o.attrs));
                        }
                        let v = Value::Record(rec);
                        match stack.last_mut() {
                            Some(parent) => parent.children.push(v),
                            None => top_level.push(v),
                        }
                    }
                }
            }
            Ok(Event::Text(t)) => {
                let s = String::from_utf8_lossy(&t).to_string();
                if let Some(o) = stack.last_mut() {
                    o.text.push_str(&s);
                }
            }
            Ok(Event::End(_)) => {
                if let Some(o) = stack.pop() {
                    let mut rec = IndexMap::new();
                    rec.insert("tag".to_string(), Value::String(o.tag));
                    if !o.attrs.is_empty() {
                        rec.insert("attrs".to_string(), Value::Record(o.attrs));
                    }
                    let txt = o.text.trim().to_string();
                    if !txt.is_empty() {
                        rec.insert("text".to_string(), Value::String(txt));
                    }
                    if !o.children.is_empty() {
                        rec.insert("children".to_string(), Value::List(o.children));
                    }
                    let v = Value::Record(rec);
                    match stack.last_mut() {
                        Some(parent) => parent.children.push(v),
                        None => top_level.push(v),
                    }
                }
            }
            Ok(Event::Eof) => break,
            Err(e) => {
                eprintln!("from-xml: {}", e);
                return Err(1);
            }
            _ => {}
        }
        buf.clear();
    }
    Ok(PipelineData::Values(top_level))
}

fn vb_to_xml(
    input: PipelineData,
    _args: &[String],
    _state: &mut ShellState,
) -> Result<PipelineData, i32> {
    let vs = input.into_values()?;
    let mut out = String::new();
    for v in &vs {
        render_xml_node(v, &mut out);
    }
    Ok(PipelineData::Bytes(out.into_bytes()))
}

fn render_xml_node(v: &Value, out: &mut String) {
    let rec = match v {
        Value::Record(r) => r,
        other => {
            out.push_str(&xml_escape(&other.to_display_string()));
            return;
        }
    };
    let tag = rec
        .get("tag")
        .map(|t| t.to_display_string())
        .unwrap_or_else(|| "item".to_string());
    out.push('<');
    out.push_str(&tag);
    if let Some(Value::Record(attrs)) = rec.get("attrs") {
        for (k, v) in attrs {
            out.push(' ');
            out.push_str(k);
            out.push('=');
            out.push('"');
            out.push_str(&xml_escape(&v.to_display_string()));
            out.push('"');
        }
    }
    let text = rec.get("text").map(|t| t.to_display_string());
    let children = match rec.get("children") {
        Some(Value::List(items)) => Some(items),
        _ => None,
    };
    if text.is_none() && children.is_none() {
        out.push_str("/>");
        return;
    }
    out.push('>');
    if let Some(t) = text {
        out.push_str(&xml_escape(&t));
    }
    if let Some(items) = children {
        for c in items {
            render_xml_node(c, out);
        }
    }
    out.push_str("</");
    out.push_str(&tag);
    out.push('>');
}

fn xml_escape(s: &str) -> String {
    s.replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
        .replace('"', "&quot;")
}

fn yaml_to_json(y: serde_yaml_ng::Value) -> serde_json::Value {
    use serde_yaml_ng::Value as Y;
    match y {
        Y::Null => serde_json::Value::Null,
        Y::Bool(b) => serde_json::Value::Bool(b),
        Y::Number(n) => {
            if let Some(i) = n.as_i64() {
                serde_json::Value::Number(i.into())
            } else if let Some(f) = n.as_f64() {
                serde_json::Number::from_f64(f)
                    .map(serde_json::Value::Number)
                    .unwrap_or(serde_json::Value::Null)
            } else {
                serde_json::Value::Null
            }
        }
        Y::String(s) => serde_json::Value::String(s),
        Y::Sequence(seq) => serde_json::Value::Array(seq.into_iter().map(yaml_to_json).collect()),
        Y::Mapping(map) => {
            let mut obj = serde_json::Map::new();
            for (k, v) in map {
                let key = match k {
                    Y::String(s) => s,
                    other => serde_yaml_ng::to_string(&other)
                        .unwrap_or_default()
                        .trim()
                        .to_string(),
                };
                obj.insert(key, yaml_to_json(v));
            }
            serde_json::Value::Object(obj)
        }
        Y::Tagged(t) => yaml_to_json(t.value),
    }
}

pub(crate) fn toml_to_json(t: toml::Value) -> serde_json::Value {
    use toml::Value as T;
    match t {
        T::String(s) => serde_json::Value::String(s),
        T::Integer(i) => serde_json::Value::Number(i.into()),
        T::Float(f) => serde_json::Number::from_f64(f)
            .map(serde_json::Value::Number)
            .unwrap_or(serde_json::Value::Null),
        T::Boolean(b) => serde_json::Value::Bool(b),
        T::Datetime(d) => serde_json::Value::String(d.to_string()),
        T::Array(arr) => serde_json::Value::Array(arr.into_iter().map(toml_to_json).collect()),
        T::Table(tbl) => {
            let mut obj = serde_json::Map::new();
            for (k, v) in tbl {
                obj.insert(k, toml_to_json(v));
            }
            serde_json::Value::Object(obj)
        }
    }
}

// ---------------------------------------------------------------------------
// Phase 5d — structured `ls` and `ps`
// ---------------------------------------------------------------------------

fn vb_ls(
    _input: PipelineData,
    args: &[String],
    _state: &mut ShellState,
) -> Result<PipelineData, i32> {
    use std::fs;
    let path = args.first().map(|s| s.as_str()).unwrap_or(".");
    let cwd = std::path::PathBuf::from(path);
    let entries = match fs::read_dir(&cwd) {
        Ok(e) => e,
        Err(e) => {
            eprintln!("ls: {}: {}", path, e);
            return Err(1);
        }
    };
    let mut out = Vec::new();
    for entry in entries.flatten() {
        let name = entry.file_name().to_string_lossy().to_string();
        if name.starts_with('.') {
            continue;
        }
        let md = match entry.metadata() {
            Ok(m) => m,
            Err(_) => continue,
        };
        let kind = if md.is_dir() {
            "dir"
        } else if md.is_symlink() {
            "link"
        } else {
            "file"
        };
        let size = md.len() as i64;
        let modified = md
            .modified()
            .ok()
            .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
            .map(|d| d.as_secs() as i64)
            .unwrap_or(0);
        let mut rec = IndexMap::new();
        rec.insert("name".to_string(), Value::String(name));
        rec.insert("type".to_string(), Value::String(kind.to_string()));
        rec.insert("size".to_string(), Value::Int(size));
        rec.insert("modified".to_string(), Value::Int(modified));
        out.push(Value::Record(rec));
    }
    out.sort_by(|a, b| {
        a.get("name")
            .map(|v| v.to_display_string())
            .cmp(&b.get("name").map(|v| v.to_display_string()))
    });
    Ok(PipelineData::Values(out))
}

fn vb_ps(
    _input: PipelineData,
    _args: &[String],
    _state: &mut ShellState,
) -> Result<PipelineData, i32> {
    use std::fs;
    let mut out = Vec::new();
    let entries = match fs::read_dir("/proc") {
        Ok(e) => e,
        Err(e) => {
            eprintln!("ps: /proc: {}", e);
            return Err(1);
        }
    };
    for entry in entries.flatten() {
        let name = entry.file_name();
        let pid_str = name.to_string_lossy();
        let pid: i64 = match pid_str.parse() {
            Ok(p) => p,
            Err(_) => continue,
        };
        let stat_path = entry.path().join("stat");
        let stat = match fs::read_to_string(&stat_path) {
            Ok(s) => s,
            Err(_) => continue,
        };
        // /proc/<pid>/stat: pid (comm) state ppid ...
        // comm can contain spaces & parens; find last ')' to split safely.
        let close = match stat.rfind(')') {
            Some(i) => i,
            None => continue,
        };
        let comm = &stat[stat.find('(').map(|i| i + 1).unwrap_or(0)..close];
        let rest: Vec<&str> = stat[close + 2..].split_whitespace().collect();
        if rest.len() < 2 {
            continue;
        }
        let state_ch = rest[0].to_string();
        let ppid: i64 = rest[1].parse().unwrap_or(0);
        let cmdline = fs::read_to_string(entry.path().join("cmdline"))
            .ok()
            .map(|s| s.replace('\0', " ").trim().to_string())
            .filter(|s| !s.is_empty())
            .unwrap_or_else(|| comm.to_string());
        let mut rec = IndexMap::new();
        rec.insert("pid".to_string(), Value::Int(pid));
        rec.insert("ppid".to_string(), Value::Int(ppid));
        rec.insert("state".to_string(), Value::String(state_ch));
        rec.insert("name".to_string(), Value::String(comm.to_string()));
        rec.insert("cmd".to_string(), Value::String(cmdline));
        out.push(Value::Record(rec));
    }
    out.sort_by_key(|v| {
        v.get("pid")
            .and_then(|x| x.as_f64())
            .map(|f| f as i64)
            .unwrap_or(0)
    });
    Ok(PipelineData::Values(out))
}

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

/// Resolve a closure reference: either the inline-expansion sentinel
/// `\x01jsh-closure:<idx>\x02` or a let-bound `Value::Closure`.
fn lookup_closure(arg: Option<&String>, state: &ShellState) -> Option<Arc<ClosureData>> {
    let a = arg?;
    if let Some(rest) = a
        .strip_prefix('\u{01}')
        .and_then(|s| s.strip_suffix('\u{02}'))
    {
        if let Some(idx) = rest
            .strip_prefix("jsh-closure:")
            .and_then(|s| s.parse::<usize>().ok())
        {
            return state.inline_closures.get(idx).cloned();
        }
    }
    if let Some(Value::Closure(c)) = state.let_vars.get(a) {
        return Some(c.clone());
    }
    None
}

fn is_truthy(v: &Value) -> bool {
    match v {
        Value::Null => false,
        Value::Bool(b) => *b,
        Value::Int(i) => *i != 0,
        Value::Float(f) => *f != 0.0,
        Value::String(s) => !s.is_empty(),
        Value::List(items) => !items.is_empty(),
        Value::Record(map) => !map.is_empty(),
        Value::Binary(b) => !b.is_empty(),
        Value::Closure(_) => true,
    }
}

fn compare(v: &Value, op: &str, rhs: &str) -> bool {
    let lhs_str = v.to_display_string();
    let lhs_num = v.as_f64();
    let rhs_num = rhs.parse::<f64>().ok();
    match op {
        "==" | "=" => lhs_str == rhs,
        "!=" => lhs_str != rhs,
        ">" | "-gt" => match (lhs_num, rhs_num) {
            (Some(l), Some(r)) => l > r,
            _ => lhs_str.as_str() > rhs,
        },
        ">=" | "-ge" => match (lhs_num, rhs_num) {
            (Some(l), Some(r)) => l >= r,
            _ => lhs_str.as_str() >= rhs,
        },
        "<" | "-lt" => match (lhs_num, rhs_num) {
            (Some(l), Some(r)) => l < r,
            _ => lhs_str.as_str() < rhs,
        },
        "<=" | "-le" => match (lhs_num, rhs_num) {
            (Some(l), Some(r)) => l <= r,
            _ => lhs_str.as_str() <= rhs,
        },
        "=~" => lhs_str.contains(rhs),
        _ => false,
    }
}

fn compare_opt(a: Option<&Value>, b: Option<&Value>) -> std::cmp::Ordering {
    use std::cmp::Ordering;
    match (a, b) {
        (None, None) => Ordering::Equal,
        (None, Some(_)) => Ordering::Less,
        (Some(_), None) => Ordering::Greater,
        (Some(va), Some(vb)) => {
            if let (Some(na), Some(nb)) = (va.as_f64(), vb.as_f64()) {
                return na.partial_cmp(&nb).unwrap_or(Ordering::Equal);
            }
            va.to_display_string().cmp(&vb.to_display_string())
        }
    }
}

fn parse_csv_line(line: &str) -> Vec<String> {
    let mut fields = Vec::new();
    let mut cur = String::new();
    let mut in_quotes = false;
    let mut chars = line.chars().peekable();
    while let Some(c) = chars.next() {
        if in_quotes {
            if c == '"' {
                if chars.peek() == Some(&'"') {
                    cur.push('"');
                    chars.next();
                } else {
                    in_quotes = false;
                }
            } else {
                cur.push(c);
            }
        } else {
            match c {
                '"' => in_quotes = true,
                ',' => {
                    fields.push(cur.trim().to_string());
                    cur = String::new();
                }
                _ => cur.push(c),
            }
        }
    }
    fields.push(cur.trim().to_string());
    fields
}

// ---------------------------------------------------------------------------
// Phase 6a — `open` / `save` (file I/O bridges to converters)
// ---------------------------------------------------------------------------

fn detect_format(path: &str) -> &'static str {
    let lower = path.to_ascii_lowercase();
    if lower.ends_with(".json") {
        "json"
    } else if lower.ends_with(".yaml") || lower.ends_with(".yml") {
        "yaml"
    } else if lower.ends_with(".toml") {
        "toml"
    } else if lower.ends_with(".xml") {
        "xml"
    } else if lower.ends_with(".csv") {
        "csv"
    } else if lower.ends_with(".txt") || lower.ends_with(".md") || lower.ends_with(".log") {
        "text"
    } else {
        "raw"
    }
}

fn vb_open(
    _input: PipelineData,
    args: &[String],
    state: &mut ShellState,
) -> Result<PipelineData, i32> {
    let path = match args.first() {
        Some(p) => p,
        None => {
            eprintln!("open: missing path");
            return Err(1);
        }
    };
    let bytes = match crate::io_guard::read_regular_file_following(
        std::path::Path::new(path),
        MAX_OPEN_FILE_BYTES,
    ) {
        Ok(b) => b,
        Err(e) => {
            eprintln!("open: {}: {}", path, e);
            return Err(1);
        }
    };
    let fmt = detect_format(path);
    let input = PipelineData::Bytes(bytes);
    match fmt {
        "json" => vb_from_json(input, &[], state),
        "yaml" => vb_from_yaml(input, &[], state),
        "toml" => vb_from_toml(input, &[], state),
        "xml" => vb_from_xml(input, &[], state),
        "csv" => vb_from_csv(input, &[], state),
        // Plain text: surface as a single String value so it can be piped to
        // `lines` / `parse` / `str ...` in Phase 6b.
        "text" => {
            let s = match input {
                PipelineData::Bytes(b) => String::from_utf8_lossy(&b).into_owned(),
                _ => String::new(),
            };
            Ok(PipelineData::Values(vec![Value::String(s)]))
        }
        // Unknown extension: passthrough bytes — external commands keep working.
        _ => Ok(input),
    }
}

fn vb_save(
    input: PipelineData,
    args: &[String],
    state: &mut ShellState,
) -> Result<PipelineData, i32> {
    let mut append = false;
    let mut path: Option<&str> = None;
    for a in args {
        if a == "--append" || a == "-a" {
            append = true;
        } else if path.is_none() {
            path = Some(a.as_str());
        }
    }
    let path = match path {
        Some(p) => p,
        None => {
            eprintln!("save: missing path");
            return Err(1);
        }
    };
    let fmt = detect_format(path);
    // Serialize input into bytes via the matching converter, then write.
    let bytes_data = match fmt {
        "json" => vb_to_json(input, &[], state)?,
        "yaml" => vb_to_yaml(input, &[], state)?,
        "toml" => vb_to_toml(input, &[], state)?,
        "xml" => vb_to_xml(input, &[], state)?,
        // text/raw/csv-fallback: write Bytes as-is; for Values, render Display.
        _ => match input {
            PipelineData::Bytes(b) => PipelineData::Bytes(b),
            PipelineData::Empty => PipelineData::Bytes(Vec::new()),
            PipelineData::Values(vs) => {
                let mut s = String::new();
                for (i, v) in vs.iter().enumerate() {
                    if i > 0 {
                        s.push('\n');
                    }
                    s.push_str(&v.to_display_string());
                }
                PipelineData::Bytes(s.into_bytes())
            }
            PipelineData::Stream(it) => {
                let vs: Vec<_> = it.collect();
                let mut s = String::new();
                for (i, v) in vs.iter().enumerate() {
                    if i > 0 {
                        s.push('\n');
                    }
                    s.push_str(&v.to_display_string());
                }
                PipelineData::Bytes(s.into_bytes())
            }
        },
    };
    let bytes = match bytes_data {
        PipelineData::Bytes(b) => b,
        _ => Vec::new(),
    };
    use std::fs::OpenOptions;
    use std::io::Write as IoWrite;
    let mut opts = OpenOptions::new();
    opts.write(true).create(true);
    if append {
        opts.append(true);
    } else {
        opts.truncate(true);
    }
    match opts.open(path) {
        Ok(mut f) => {
            if let Err(e) = f.write_all(&bytes) {
                eprintln!("save: {}: {}", path, e);
                return Err(1);
            }
        }
        Err(e) => {
            eprintln!("save: {}: {}", path, e);
            return Err(1);
        }
    }
    Ok(PipelineData::Empty)
}

// ---------------------------------------------------------------------------
// Phase 6b — text → structured bridges
// ---------------------------------------------------------------------------

fn input_as_string(input: PipelineData) -> Result<String, i32> {
    let input = match input {
        PipelineData::Stream(it) => PipelineData::Values(it.collect()),
        other => other,
    };
    match input {
        PipelineData::Empty => Ok(String::new()),
        PipelineData::Bytes(b) => Ok(String::from_utf8_lossy(&b).into_owned()),
        PipelineData::Values(vs) => {
            if vs.len() == 1 {
                if let Value::String(s) = &vs[0] {
                    return Ok(s.clone());
                }
            }
            let mut s = String::new();
            for (i, v) in vs.iter().enumerate() {
                if i > 0 {
                    s.push('\n');
                }
                s.push_str(&v.to_display_string());
            }
            Ok(s)
        }
        PipelineData::Stream(_) => unreachable!("normalized above"),
    }
}

fn vb_lines(
    input: PipelineData,
    _args: &[String],
    _state: &mut ShellState,
) -> Result<PipelineData, i32> {
    let s = input_as_string(input)?;
    // Drop trailing empty line so `echo "a\nb\n"` yields ["a","b"], not ["a","b",""].
    let mut out: Vec<Value> = s
        .split('\n')
        .map(|l| Value::String(l.to_string()))
        .collect();
    if let Some(Value::String(last)) = out.last() {
        if last.is_empty() {
            out.pop();
        }
    }
    Ok(PipelineData::Values(out))
}

fn vb_split(
    input: PipelineData,
    args: &[String],
    _state: &mut ShellState,
) -> Result<PipelineData, i32> {
    let sub = match args.first().map(|s| s.as_str()) {
        Some("row") => "row",
        Some("column") => "column",
        _ => {
            eprintln!("split: expected 'row' or 'column' subcommand");
            return Err(1);
        }
    };
    let rest = &args[1..];
    let sep = match rest.first() {
        Some(s) => s.clone(),
        None => {
            eprintln!("split {}: missing separator", sub);
            return Err(1);
        }
    };
    let input = match input {
        PipelineData::Stream(it) => PipelineData::Values(it.collect()),
        other => other,
    };
    if sub == "row" {
        match input {
            PipelineData::Empty => Ok(PipelineData::Values(Vec::new())),
            PipelineData::Bytes(b) => {
                let s = String::from_utf8_lossy(&b);
                Ok(PipelineData::Values(
                    s.split(sep.as_str())
                        .map(|x| Value::String(x.to_string()))
                        .collect(),
                ))
            }
            PipelineData::Values(vs) => {
                let mut out = Vec::new();
                for v in vs {
                    let s = v.to_display_string();
                    for piece in s.split(sep.as_str()) {
                        out.push(Value::String(piece.to_string()));
                    }
                }
                Ok(PipelineData::Values(out))
            }
            PipelineData::Stream(_) => unreachable!("normalized above"),
        }
    } else {
        // split column SEP NAME...
        let names: Vec<String> = rest[1..].to_vec();
        let process = |line: &str| -> Value {
            let parts: Vec<&str> = line.split(sep.as_str()).collect();
            let mut rec = IndexMap::new();
            for (i, p) in parts.iter().enumerate() {
                let key = names
                    .get(i)
                    .cloned()
                    .unwrap_or_else(|| format!("column{}", i + 1));
                rec.insert(key, Value::String(p.to_string()));
            }
            Value::Record(rec)
        };
        match input {
            PipelineData::Empty => Ok(PipelineData::Values(Vec::new())),
            PipelineData::Bytes(b) => {
                let s = String::from_utf8_lossy(&b).into_owned();
                Ok(PipelineData::Values(vec![process(&s)]))
            }
            PipelineData::Values(vs) => {
                let out: Vec<Value> = vs.iter().map(|v| process(&v.to_display_string())).collect();
                Ok(PipelineData::Values(out))
            }
            PipelineData::Stream(_) => unreachable!("normalized above"),
        }
    }
}

fn vb_parse(
    input: PipelineData,
    args: &[String],
    _state: &mut ShellState,
) -> Result<PipelineData, i32> {
    // `parse [-r|--regex] <pattern>`. With -r the pattern is a raw regex with
    // named captures; without -r the pattern uses `{field}` templates.
    let mut use_regex = false;
    let mut positional: Vec<&String> = Vec::new();
    for a in args {
        match a.as_str() {
            "-r" | "--regex" => use_regex = true,
            _ => positional.push(a),
        }
    }
    let pat = match positional.first() {
        Some(p) => *p,
        None => {
            eprintln!("parse: missing pattern");
            return Err(1);
        }
    };
    let (rx, names) = if use_regex {
        let rx = match regex::Regex::new(pat) {
            Ok(r) => r,
            Err(e) => {
                eprintln!("parse: bad regex: {}", e);
                return Err(1);
            }
        };
        let names: Vec<String> = rx
            .capture_names()
            .enumerate()
            .filter_map(|(i, n)| {
                n.map(|s| s.to_string()).or_else(|| {
                    if i > 0 {
                        Some(format!("capture{}", i))
                    } else {
                        None
                    }
                })
            })
            .collect();
        (rx, names)
    } else {
        match template_to_regex(pat) {
            Ok(p) => p,
            Err(e) => {
                eprintln!("parse: bad template: {}", e);
                return Err(1);
            }
        }
    };
    let lines: Vec<String> = match input {
        PipelineData::Empty => Vec::new(),
        PipelineData::Bytes(b) => String::from_utf8_lossy(&b)
            .lines()
            .map(|s| s.to_string())
            .collect(),
        PipelineData::Values(vs) => vs.iter().map(|v| v.to_display_string()).collect(),
        PipelineData::Stream(it) => it.map(|v| v.to_display_string()).collect(),
    };
    let mut out = Vec::new();
    for line in &lines {
        if let Some(caps) = rx.captures(line) {
            let mut rec = IndexMap::new();
            for (i, name) in names.iter().enumerate() {
                // Prefer named-capture lookup; fall back to positional index for
                // anonymous capture groups produced by `-r` mode.
                let val = caps
                    .name(name)
                    .or_else(|| caps.get(i + 1))
                    .map(|m| m.as_str().to_string())
                    .unwrap_or_default();
                rec.insert(name.clone(), Value::String(val));
            }
            out.push(Value::Record(rec));
        }
    }
    Ok(PipelineData::Values(out))
}

fn template_to_regex(template: &str) -> Result<(regex::Regex, Vec<String>), String> {
    let mut rx = String::from("^");
    let mut names = Vec::new();
    let mut chars = template.chars().peekable();
    while let Some(c) = chars.next() {
        if c == '{' {
            let mut name = String::new();
            while let Some(&nc) = chars.peek() {
                chars.next();
                if nc == '}' {
                    break;
                }
                name.push(nc);
            }
            if name.is_empty() {
                return Err("empty {} capture".into());
            }
            rx.push_str(&format!("(?P<{}>.*?)", name));
            names.push(name);
        } else {
            // Escape regex metachars
            if "\\.+*?^$()[]|".contains(c) {
                rx.push('\\');
            }
            rx.push(c);
        }
    }
    rx.push('$');
    regex::Regex::new(&rx)
        .map(|r| (r, names))
        .map_err(|e| e.to_string())
}

fn vb_str(
    input: PipelineData,
    args: &[String],
    _state: &mut ShellState,
) -> Result<PipelineData, i32> {
    let sub = match args.first().map(|s| s.as_str()) {
        Some(s) => s,
        None => {
            eprintln!("str: missing subcommand");
            return Err(1);
        }
    };
    let rest = &args[1..];
    let map_str = |s: &str| -> Result<String, i32> {
        Ok(match sub {
            "trim" => s.trim().to_string(),
            "upcase" | "upper" => s.to_uppercase(),
            "downcase" | "lower" => s.to_lowercase(),
            "length" => return Ok(s.chars().count().to_string()),
            "contains" => {
                let needle = rest.first().map(|x| x.as_str()).unwrap_or("");
                return Ok(s.contains(needle).to_string());
            }
            "replace" => {
                // Optional flags: `-r`/`--regex` for regex mode, `-a`/`--all`
                // for replace-all (default is first occurrence only in regex
                // mode; literal `replace` always replaces all to preserve
                // backwards compatibility).
                let mut use_regex = false;
                let mut all = false;
                let mut positional: Vec<&String> = Vec::new();
                for a in rest {
                    match a.as_str() {
                        "-r" | "--regex" => use_regex = true,
                        "-a" | "--all" => all = true,
                        _ => positional.push(a),
                    }
                }
                if positional.len() < 2 {
                    eprintln!("str replace: need <from> <to>");
                    return Err(1);
                }
                if use_regex {
                    let rx = match regex::Regex::new(positional[0]) {
                        Ok(r) => r,
                        Err(e) => {
                            eprintln!("str replace: bad regex: {}", e);
                            return Err(1);
                        }
                    };
                    if all {
                        rx.replace_all(s, positional[1].as_str()).into_owned()
                    } else {
                        rx.replace(s, positional[1].as_str()).into_owned()
                    }
                } else {
                    s.replace(positional[0].as_str(), positional[1].as_str())
                }
            }
            "split" => {
                let sep = rest.first().map(|x| x.as_str()).unwrap_or(" ");
                return Ok(format!(
                    "__jsh_split__{}",
                    s.split(sep).collect::<Vec<_>>().join("\x1f")
                ));
            }
            "starts-with" => {
                let p = rest.first().map(|x| x.as_str()).unwrap_or("");
                return Ok(s.starts_with(p).to_string());
            }
            "ends-with" => {
                let p = rest.first().map(|x| x.as_str()).unwrap_or("");
                return Ok(s.ends_with(p).to_string());
            }
            "index-of" => {
                let needle = rest.first().map(|x| x.as_str()).unwrap_or("");
                return Ok(s.find(needle).map(|i| i as i64).unwrap_or(-1).to_string());
            }
            "pad-left" | "pad-right" => {
                let width: usize = rest.first().and_then(|x| x.parse().ok()).unwrap_or(0);
                let ch = rest.get(1).and_then(|x| x.chars().next()).unwrap_or(' ');
                let need = width.saturating_sub(s.chars().count());
                let pad: String = std::iter::repeat_n(ch, need).collect();
                if sub == "pad-left" {
                    format!("{}{}", pad, s)
                } else {
                    format!("{}{}", s, pad)
                }
            }
            "reverse" => s.chars().rev().collect(),
            "distance" => {
                let other = rest.first().map(|x| x.as_str()).unwrap_or("");
                return Ok(levenshtein(s, other).to_string());
            }
            _ => {
                eprintln!("str: unknown subcommand '{}'", sub);
                return Err(1);
            }
        })
    };
    // Apply per-Value. For Bytes, treat as single string.
    let coerce = |r: String| -> Value {
        if let Some(rest_s) = r.strip_prefix("__jsh_split__") {
            Value::List(
                rest_s
                    .split('\x1f')
                    .map(|p| Value::String(p.to_string()))
                    .collect(),
            )
        } else if matches!(sub, "length" | "index-of" | "distance") {
            Value::Int(r.parse().unwrap_or(0))
        } else if matches!(sub, "contains" | "starts-with" | "ends-with") {
            Value::Bool(r == "true")
        } else {
            Value::String(r)
        }
    };
    let convert = |v: Value| -> Result<Value, i32> {
        let s = match v {
            Value::String(s) => s,
            other => other.to_display_string(),
        };
        Ok(coerce(map_str(&s)?))
    };
    match input {
        PipelineData::Empty => Ok(PipelineData::Empty),
        PipelineData::Bytes(b) => {
            // Bytes from upstream commands typically carry a trailing newline
            // (`echo foo` → "foo\n"). Strip it so `str length`/`str ends-with`
            // line up with nushell semantics.
            let s = String::from_utf8_lossy(&b);
            let trimmed = s.strip_suffix('\n').unwrap_or(&s).to_string();
            let v = Value::String(trimmed);
            Ok(PipelineData::Values(vec![convert(v)?]))
        }
        PipelineData::Values(vs) => {
            let mut out = Vec::with_capacity(vs.len());
            for v in vs {
                out.push(convert(v)?);
            }
            Ok(PipelineData::Values(out))
        }
        PipelineData::Stream(it) => {
            let mut out = Vec::new();
            for v in it {
                out.push(convert(v)?);
            }
            Ok(PipelineData::Values(out))
        }
    }
}

// ---------------------------------------------------------------------------
// Phase 6c — table / record operators (`get`, `update`, `insert`, `reject`,
// `wrap`, `flatten`)
// ---------------------------------------------------------------------------

fn parse_cell_path(s: &str) -> Vec<crate::parser::ast::PathSeg> {
    use crate::parser::ast::PathSeg;
    let mut out = Vec::new();
    let mut cur = String::new();
    let mut in_bracket = false;
    let flush = |cur: &mut String, out: &mut Vec<PathSeg>| {
        if cur.is_empty() {
            return;
        }
        // Bare integer segment → Index (nushell-style `0.a.b`).
        if let Ok(n) = cur.parse::<i64>() {
            out.push(PathSeg::Index(n));
        } else {
            out.push(PathSeg::Field(std::mem::take(cur)));
        }
        cur.clear();
    };
    for c in s.chars() {
        match c {
            '.' if !in_bracket => flush(&mut cur, &mut out),
            '[' => {
                flush(&mut cur, &mut out);
                in_bracket = true;
            }
            ']' => {
                if in_bracket {
                    if let Ok(n) = cur.parse::<i64>() {
                        out.push(PathSeg::Index(n));
                    }
                    cur.clear();
                    in_bracket = false;
                }
            }
            _ => cur.push(c),
        }
    }
    flush(&mut cur, &mut out);
    out
}

fn resolve_cell_path(v: &Value, path: &[crate::parser::ast::PathSeg]) -> Option<Value> {
    use crate::parser::ast::PathSeg;
    let mut cur = v.clone();
    for seg in path {
        cur = match (seg, &cur) {
            (PathSeg::Field(k), Value::Record(r)) => r.get(k).cloned()?,
            (PathSeg::Index(i), Value::List(items)) => {
                let len = items.len() as i64;
                let idx = if *i < 0 { len + *i } else { *i };
                if idx < 0 || idx as usize >= items.len() {
                    return None;
                }
                items[idx as usize].clone()
            }
            _ => return None,
        };
    }
    Some(cur)
}

fn vb_get(
    input: PipelineData,
    args: &[String],
    _state: &mut ShellState,
) -> Result<PipelineData, i32> {
    use crate::parser::ast::PathSeg;
    // `get [-i|--ignore-errors] <path>` — with -i, missing path returns Null
    // instead of erroring (matches nushell's `get -i` / `get --optional`).
    let mut ignore = false;
    let mut path_arg: Option<&String> = None;
    for a in args {
        match a.as_str() {
            "-i" | "--ignore-errors" | "--optional" => ignore = true,
            _ => {
                if path_arg.is_none() {
                    path_arg = Some(a);
                }
            }
        }
    }
    let path_str = match path_arg {
        Some(p) => p,
        None => {
            eprintln!("get: missing cell-path");
            return Err(1);
        }
    };
    let path = parse_cell_path(path_str);
    let vs = input.into_values()?;
    // Path that begins with an Index treats the pipeline as a List(vs); other
    // paths drill into a single Value or project a column from each row.
    let starts_with_index = matches!(path.first(), Some(PathSeg::Index(_)));
    let suggest_field = |state: &mut ShellState, vs: &[Value]| {
        // Use only the first key in the path for the suggestion — that's the
        // most useful signal when the user typo'd a top-level field.
        let first_key = path.iter().find_map(|seg| match seg {
            PathSeg::Field(s) => Some(s.as_str()),
            _ => None,
        });
        if let Some(typed) = first_key {
            let keys = record_keys_from_input(vs);
            if let Some(sug) = closest_match(typed, keys.iter().map(|s| s.as_str())) {
                let msg = format!(
                    "get: path '{}' not found, did you mean '{}'?",
                    path_str, sug
                );
                eprintln!("{}", msg);
                state.set_error(msg, 1);
                return;
            }
        }
        let msg = format!("get: path '{}' not found", path_str);
        eprintln!("{}", msg);
        state.set_error(msg, 1);
    };
    if starts_with_index {
        let v = Value::List(vs.clone());
        match resolve_cell_path(&v, &path) {
            Some(r) => Ok(PipelineData::Values(vec![r])),
            None => {
                if ignore {
                    Ok(PipelineData::Values(vec![Value::Null]))
                } else {
                    suggest_field(_state, &vs);
                    Err(1)
                }
            }
        }
    } else if vs.len() == 1 {
        match resolve_cell_path(&vs[0], &path) {
            Some(v) => Ok(PipelineData::Values(vec![v])),
            None => {
                if ignore {
                    Ok(PipelineData::Values(vec![Value::Null]))
                } else {
                    suggest_field(_state, &vs);
                    Err(1)
                }
            }
        }
    } else {
        let mut out = Vec::with_capacity(vs.len());
        for v in vs {
            match resolve_cell_path(&v, &path) {
                Some(x) => out.push(x),
                None => out.push(Value::Null),
            }
        }
        Ok(PipelineData::Values(out))
    }
}

fn vb_update(
    input: PipelineData,
    args: &[String],
    state: &mut ShellState,
) -> Result<PipelineData, i32> {
    if args.len() < 2 {
        eprintln!("update: usage `update <col> <value-or-closure>`");
        return Err(1);
    }
    let col = &args[0];
    let new_arg = &args[1];
    let closure = lookup_closure(Some(new_arg), state);
    let vs = input.into_values()?;
    let mut out = Vec::with_capacity(vs.len());
    for v in vs {
        let updated = if let Value::Record(mut r) = v {
            let new_val = if let Some(ref c) = closure {
                let row = Value::Record(r.clone());
                crate::executor::apply_closure(c, std::slice::from_ref(&row), state)?
            } else {
                parse_literal_value(new_arg)
            };
            r.insert(col.clone(), new_val);
            Value::Record(r)
        } else {
            v
        };
        out.push(updated);
    }
    Ok(PipelineData::Values(out))
}

fn parse_literal_value(s: &str) -> Value {
    if let Ok(n) = s.parse::<i64>() {
        return Value::Int(n);
    }
    if let Ok(f) = s.parse::<f64>() {
        return Value::Float(f);
    }
    match s {
        "true" => Value::Bool(true),
        "false" => Value::Bool(false),
        "null" => Value::Null,
        _ => Value::String(s.to_string()),
    }
}

fn vb_insert(
    input: PipelineData,
    args: &[String],
    state: &mut ShellState,
) -> Result<PipelineData, i32> {
    // Same as update but errors if column already exists. Closure form supported.
    if args.len() < 2 {
        eprintln!("insert: usage `insert <col> <value-or-closure>`");
        return Err(1);
    }
    let col = &args[0];
    let new_arg = &args[1];
    let closure = lookup_closure(Some(new_arg), state);
    let vs = input.into_values()?;
    let mut out = Vec::with_capacity(vs.len());
    for v in vs {
        let inserted = if let Value::Record(mut r) = v {
            if r.contains_key(col) {
                eprintln!("insert: column '{}' already exists", col);
                return Err(1);
            }
            let new_val = if let Some(ref c) = closure {
                let row = Value::Record(r.clone());
                crate::executor::apply_closure(c, std::slice::from_ref(&row), state)?
            } else {
                parse_literal_value(new_arg)
            };
            r.insert(col.clone(), new_val);
            Value::Record(r)
        } else {
            v
        };
        out.push(inserted);
    }
    Ok(PipelineData::Values(out))
}

fn vb_reject(
    input: PipelineData,
    args: &[String],
    _state: &mut ShellState,
) -> Result<PipelineData, i32> {
    if args.is_empty() {
        eprintln!("reject: missing column names");
        return Err(1);
    }
    let vs = input.into_values()?;
    let mut out = Vec::with_capacity(vs.len());
    for v in vs {
        let pruned = if let Value::Record(mut r) = v {
            for col in args {
                r.shift_remove(col);
            }
            Value::Record(r)
        } else {
            v
        };
        out.push(pruned);
    }
    Ok(PipelineData::Values(out))
}

fn vb_wrap(
    input: PipelineData,
    args: &[String],
    _state: &mut ShellState,
) -> Result<PipelineData, i32> {
    let col = match args.first() {
        Some(c) => c.clone(),
        None => {
            eprintln!("wrap: missing column name");
            return Err(1);
        }
    };
    let vs = input.into_values()?;
    let out: Vec<Value> = vs
        .into_iter()
        .map(|v| {
            let mut r = IndexMap::new();
            r.insert(col.clone(), v);
            Value::Record(r)
        })
        .collect();
    Ok(PipelineData::Values(out))
}

fn vb_flatten(
    input: PipelineData,
    _args: &[String],
    _state: &mut ShellState,
) -> Result<PipelineData, i32> {
    let vs = input.into_values()?;
    let mut out = Vec::new();
    for v in vs {
        match v {
            Value::List(items) => out.extend(items),
            other => out.push(other),
        }
    }
    Ok(PipelineData::Values(out))
}

// ---------------------------------------------------------------------------
// Phase 6d — type conversions and ranges
// ---------------------------------------------------------------------------

fn vb_into(
    input: PipelineData,
    args: &[String],
    _state: &mut ShellState,
) -> Result<PipelineData, i32> {
    let target = match args.first().map(|s| s.as_str()) {
        Some(t) => t,
        None => {
            eprintln!("into: missing type (int|float|string|bool)");
            return Err(1);
        }
    };
    let col = args.get(1).map(|s| s.as_str());
    let convert = |v: &Value| -> Value {
        match target {
            "int" => match v {
                Value::Int(n) => Value::Int(*n),
                Value::Float(f) => Value::Int(*f as i64),
                Value::Bool(b) => Value::Int(if *b { 1 } else { 0 }),
                Value::String(s) => s
                    .trim()
                    .parse::<i64>()
                    .ok()
                    .or_else(|| s.trim().parse::<f64>().ok().map(|f| f as i64))
                    .map(Value::Int)
                    .unwrap_or(Value::Null),
                _ => Value::Null,
            },
            "float" => match v {
                Value::Int(n) => Value::Float(*n as f64),
                Value::Float(f) => Value::Float(*f),
                Value::String(s) => s
                    .trim()
                    .parse::<f64>()
                    .ok()
                    .map(Value::Float)
                    .unwrap_or(Value::Null),
                _ => Value::Null,
            },
            "string" => Value::String(v.to_display_string()),
            "bool" => match v {
                Value::Bool(b) => Value::Bool(*b),
                Value::Int(n) => Value::Bool(*n != 0),
                Value::Float(f) => Value::Bool(*f != 0.0),
                Value::String(s) => Value::Bool(!s.is_empty() && s != "false" && s != "0"),
                Value::Null => Value::Bool(false),
                _ => Value::Bool(true),
            },
            _ => v.clone(),
        }
    };
    let vs = input.into_values()?;
    let out: Vec<Value> = vs
        .into_iter()
        .map(|v| match (col, &v) {
            (Some(c), Value::Record(r)) => {
                let mut new_r = r.clone();
                if let Some(cell) = r.get(c) {
                    new_r.insert(c.to_string(), convert(cell));
                }
                Value::Record(new_r)
            }
            _ => convert(&v),
        })
        .collect();
    Ok(PipelineData::Values(out))
}

fn vb_range(
    _input: PipelineData,
    args: &[String],
    _state: &mut ShellState,
) -> Result<PipelineData, i32> {
    let spec = match args.first() {
        Some(s) => s,
        None => {
            eprintln!("range: usage `range N..M` (inclusive) or `range N..<M` (exclusive)");
            return Err(1);
        }
    };
    let (lo, hi, inclusive) = if let Some((a, b)) = spec.split_once("..<") {
        (a.parse::<i64>().ok(), b.parse::<i64>().ok(), false)
    } else if let Some((a, b)) = spec.split_once("..") {
        (a.parse::<i64>().ok(), b.parse::<i64>().ok(), true)
    } else {
        eprintln!("range: bad spec '{}'", spec);
        return Err(1);
    };
    let (lo, hi) = match (lo, hi) {
        (Some(a), Some(b)) => (a, b),
        _ => {
            eprintln!("range: bounds must be integers");
            return Err(1);
        }
    };
    let end = if inclusive { hi + 1 } else { hi };
    // Phase 15b: return a lazy stream so `range 1..10_000_000 | take 5`
    // doesn't have to materialize 10M ints first.
    let it = (lo..end).map(Value::Int);
    Ok(PipelineData::Stream(Box::new(it)))
}

// ---------------------------------------------------------------------------
// Phase 7a — iteration combinators
// ---------------------------------------------------------------------------

fn vb_reduce(
    input: PipelineData,
    args: &[String],
    state: &mut ShellState,
) -> Result<PipelineData, i32> {
    // Forms: `reduce {|acc, it| ...}` (no init; first element is acc)
    //        `reduce -i <init> {|acc, it| ...}` (with literal initial value)
    let (init, closure_arg) = if args.first().map(|s| s.as_str()) == Some("-i") {
        if args.len() < 3 {
            eprintln!("reduce: usage `reduce [-i <init>] {{|acc, it| ...}}`");
            return Err(1);
        }
        (Some(parse_literal_value(&args[1])), Some(&args[2]))
    } else {
        (None, args.first())
    };
    let closure = match lookup_closure(closure_arg, state) {
        Some(c) => c,
        None => {
            eprintln!("reduce: missing closure");
            return Err(1);
        }
    };
    let vs = input.into_values()?;
    let mut iter = vs.into_iter();
    let mut acc = match init {
        Some(v) => v,
        None => match iter.next() {
            Some(v) => v,
            None => return Ok(PipelineData::Values(vec![Value::Null])),
        },
    };
    for it in iter {
        acc = crate::executor::apply_closure(&closure, &[acc, it], state)?;
    }
    Ok(PipelineData::Values(vec![acc]))
}

fn vb_take(
    input: PipelineData,
    args: &[String],
    _state: &mut ShellState,
) -> Result<PipelineData, i32> {
    let n: usize = args.first().and_then(|s| s.parse().ok()).unwrap_or(1);
    // Phase 15b: short-circuit on Stream — pull only N items.
    if input.is_stream() {
        let it = input.into_value_iter()?.take(n);
        let vs: Vec<Value> = it.collect();
        return Ok(PipelineData::Values(vs));
    }
    let mut vs = input.into_values()?;
    vs.truncate(n);
    Ok(PipelineData::Values(vs))
}

fn vb_skip(
    input: PipelineData,
    args: &[String],
    _state: &mut ShellState,
) -> Result<PipelineData, i32> {
    let n: usize = args.first().and_then(|s| s.parse().ok()).unwrap_or(1);
    let vs = input.into_values()?;
    Ok(PipelineData::Values(vs.into_iter().skip(n).collect()))
}

fn vb_enumerate(
    input: PipelineData,
    _args: &[String],
    _state: &mut ShellState,
) -> Result<PipelineData, i32> {
    let vs = input.into_values()?;
    let out: Vec<Value> = vs
        .into_iter()
        .enumerate()
        .map(|(i, item)| {
            let mut r = IndexMap::new();
            r.insert("index".to_string(), Value::Int(i as i64));
            r.insert("item".to_string(), item);
            Value::Record(r)
        })
        .collect();
    Ok(PipelineData::Values(out))
}

fn vb_zip(
    input: PipelineData,
    args: &[String],
    _state: &mut ShellState,
) -> Result<PipelineData, i32> {
    // `zip <json-list>` — pair pipeline values with values from a literal JSON list.
    let other_arg = match args.first() {
        Some(s) => s,
        None => {
            eprintln!("zip: missing other list (JSON literal)");
            return Err(1);
        }
    };
    let other: Vec<Value> = match serde_json::from_str::<serde_json::Value>(other_arg) {
        Ok(serde_json::Value::Array(arr)) => arr.into_iter().map(Value::from_json).collect(),
        _ => {
            eprintln!("zip: argument must be a JSON list");
            return Err(1);
        }
    };
    let vs = input.into_values()?;
    let out: Vec<Value> = vs
        .into_iter()
        .zip(other)
        .map(|(a, b)| Value::List(vec![a, b]))
        .collect();
    Ok(PipelineData::Values(out))
}

// ---------------------------------------------------------------------------
// Phase 7b — record/table shaping
// ---------------------------------------------------------------------------

fn vb_columns(
    input: PipelineData,
    _args: &[String],
    _state: &mut ShellState,
) -> Result<PipelineData, i32> {
    let vs = input.into_values()?;
    // Use first record (or only value) as the column source.
    let first = vs.into_iter().next().unwrap_or(Value::Null);
    let cols: Vec<Value> = match first {
        Value::Record(m) => m.into_iter().map(|(k, _)| Value::String(k)).collect(),
        _ => Vec::new(),
    };
    Ok(PipelineData::Values(cols))
}

fn vb_values(
    input: PipelineData,
    _args: &[String],
    _state: &mut ShellState,
) -> Result<PipelineData, i32> {
    let vs = input.into_values()?;
    let first = vs.into_iter().next().unwrap_or(Value::Null);
    let out: Vec<Value> = match first {
        Value::Record(m) => m.into_iter().map(|(_, v)| v).collect(),
        Value::List(xs) => xs,
        other => vec![other],
    };
    Ok(PipelineData::Values(out))
}

fn vb_rename(
    input: PipelineData,
    args: &[String],
    _state: &mut ShellState,
) -> Result<PipelineData, i32> {
    if args.len() < 2 {
        eprintln!("rename: usage `rename <old> <new>`");
        return Err(1);
    }
    let old = &args[0];
    let new = &args[1];
    let vs = input.into_values()?;
    let out: Vec<Value> = vs
        .into_iter()
        .map(|v| match v {
            Value::Record(m) => {
                let mut nm = IndexMap::new();
                for (k, val) in m {
                    if &k == old {
                        nm.insert(new.clone(), val);
                    } else {
                        nm.insert(k, val);
                    }
                }
                Value::Record(nm)
            }
            other => other,
        })
        .collect();
    Ok(PipelineData::Values(out))
}

fn vb_move(
    input: PipelineData,
    args: &[String],
    _state: &mut ShellState,
) -> Result<PipelineData, i32> {
    // `move <col> --before <other>` or `move <col> --after <other>`
    if args.len() < 3 {
        eprintln!("move: usage `move <col> --before|--after <other>`");
        return Err(1);
    }
    let col = &args[0];
    let mode = &args[1];
    let anchor = &args[2];
    let before = match mode.as_str() {
        "--before" => true,
        "--after" => false,
        _ => {
            eprintln!("move: expected --before or --after");
            return Err(1);
        }
    };
    let vs = input.into_values()?;
    let out: Vec<Value> = vs
        .into_iter()
        .map(|v| match v {
            Value::Record(mut m) => {
                let val = match m.shift_remove(col) {
                    Some(v) => v,
                    None => return Value::Record(m),
                };
                let mut nm = IndexMap::new();
                let mut inserted = false;
                for (k, v) in m {
                    if !inserted && &k == anchor && before {
                        nm.insert(col.clone(), val.clone());
                        inserted = true;
                    }
                    let is_anchor = &k == anchor;
                    nm.insert(k, v);
                    if !inserted && is_anchor && !before {
                        nm.insert(col.clone(), val.clone());
                        inserted = true;
                    }
                }
                if !inserted {
                    nm.insert(col.clone(), val);
                }
                Value::Record(nm)
            }
            other => other,
        })
        .collect();
    Ok(PipelineData::Values(out))
}

fn vb_merge(
    input: PipelineData,
    args: &[String],
    _state: &mut ShellState,
) -> Result<PipelineData, i32> {
    // `merge <json-record-or-table>` — per-row merge for table inputs;
    // for a single record input, merge the literal record into it.
    let other_arg = match args.first() {
        Some(s) => s,
        None => {
            eprintln!("merge: missing other (JSON record or table)");
            return Err(1);
        }
    };
    let other_json: serde_json::Value = match serde_json::from_str(other_arg) {
        Ok(v) => v,
        Err(_) => {
            eprintln!("merge: argument must be JSON");
            return Err(1);
        }
    };
    let vs = input.into_values()?;
    let merge_one = |a: Value, b: &Value| -> Value {
        match (a, b) {
            (Value::Record(mut ma), Value::Record(mb)) => {
                for (k, v) in mb {
                    ma.insert(k.clone(), v.clone());
                }
                Value::Record(ma)
            }
            (a, _) => a,
        }
    };
    let out: Vec<Value> = match other_json {
        serde_json::Value::Array(arr) => {
            let others: Vec<Value> = arr.into_iter().map(Value::from_json).collect();
            vs.into_iter()
                .zip(others)
                .map(|(a, b)| merge_one(a, &b))
                .collect()
        }
        other => {
            let bv = Value::from_json(other);
            vs.into_iter().map(|a| merge_one(a, &bv)).collect()
        }
    };
    Ok(PipelineData::Values(out))
}

fn vb_upsert(
    input: PipelineData,
    args: &[String],
    state: &mut ShellState,
) -> Result<PipelineData, i32> {
    if args.len() < 2 {
        eprintln!("upsert: usage `upsert <col> <value-or-closure>`");
        return Err(1);
    }
    let col = &args[0];
    let new_arg = &args[1];
    let closure = lookup_closure(Some(new_arg), state);
    let vs = input.into_values()?;
    let mut out = Vec::with_capacity(vs.len());
    for v in vs {
        let upserted = if let Value::Record(mut r) = v {
            let new_val = if let Some(ref c) = closure {
                let row = Value::Record(r.clone());
                crate::executor::apply_closure(c, std::slice::from_ref(&row), state)?
            } else {
                parse_literal_value(new_arg)
            };
            r.insert(col.clone(), new_val);
            Value::Record(r)
        } else {
            v
        };
        out.push(upserted);
    }
    Ok(PipelineData::Values(out))
}

fn vb_compact(
    input: PipelineData,
    args: &[String],
    _state: &mut ShellState,
) -> Result<PipelineData, i32> {
    // Without args: drop records containing any Null value, and drop Null elements
    // from lists. With column args: drop records where any named column is Null/missing.
    let vs = input.into_values()?;
    let out: Vec<Value> = vs
        .into_iter()
        .filter(|v| match v {
            Value::Null => false,
            Value::Record(m) => {
                if args.is_empty() {
                    m.values().all(|x| !matches!(x, Value::Null))
                } else {
                    args.iter()
                        .all(|c| matches!(m.get(c), Some(x) if !matches!(x, Value::Null)))
                }
            }
            _ => true,
        })
        .collect();
    Ok(PipelineData::Values(out))
}

// ---------------------------------------------------------------------------
// Phase 7c — predicates / reflection
// ---------------------------------------------------------------------------

fn vb_any(
    input: PipelineData,
    args: &[String],
    state: &mut ShellState,
) -> Result<PipelineData, i32> {
    let closure = match lookup_closure(args.first(), state) {
        Some(c) => c,
        None => {
            eprintln!("any: usage `any {{|r| ...}}`");
            return Err(1);
        }
    };
    let vs = input.into_values()?;
    for v in vs {
        let r = crate::executor::apply_closure(&closure, std::slice::from_ref(&v), state)?;
        if is_truthy(&r) {
            return Ok(PipelineData::Values(vec![Value::Bool(true)]));
        }
    }
    Ok(PipelineData::Values(vec![Value::Bool(false)]))
}

fn vb_all(
    input: PipelineData,
    args: &[String],
    state: &mut ShellState,
) -> Result<PipelineData, i32> {
    let closure = match lookup_closure(args.first(), state) {
        Some(c) => c,
        None => {
            eprintln!("all: usage `all {{|r| ...}}`");
            return Err(1);
        }
    };
    let vs = input.into_values()?;
    for v in vs {
        let r = crate::executor::apply_closure(&closure, std::slice::from_ref(&v), state)?;
        if !is_truthy(&r) {
            return Ok(PipelineData::Values(vec![Value::Bool(false)]));
        }
    }
    Ok(PipelineData::Values(vec![Value::Bool(true)]))
}

fn vb_is_empty(
    input: PipelineData,
    _args: &[String],
    _state: &mut ShellState,
) -> Result<PipelineData, i32> {
    let vs = input.into_values()?;
    let empty = match vs.as_slice() {
        [] => true,
        [v] => match v {
            Value::Null => true,
            Value::String(s) => s.is_empty(),
            Value::List(x) => x.is_empty(),
            Value::Record(m) => m.is_empty(),
            Value::Binary(b) => b.is_empty(),
            _ => false,
        },
        _ => false,
    };
    Ok(PipelineData::Values(vec![Value::Bool(empty)]))
}

fn vb_describe(
    input: PipelineData,
    _args: &[String],
    _state: &mut ShellState,
) -> Result<PipelineData, i32> {
    let vs = input.into_values()?;
    // The pipeline model unwraps top-level JSON arrays into multiple Values, so
    // `[{"a":1}]` and `{"a":1}` both arrive here as `[Record]`. Disambiguate by
    // size: multi-value pipelines are list/table; single-value reports its own
    // type; empty is "nothing".
    let name = if vs.is_empty() {
        "nothing".to_string()
    } else if vs.iter().all(|x| matches!(x, Value::Record(_))) {
        "table".to_string()
    } else if vs.len() == 1 {
        match &vs[0] {
            Value::Null => "nothing",
            Value::Bool(_) => "bool",
            Value::Int(_) => "int",
            Value::Float(_) => "float",
            Value::String(_) => "string",
            Value::Binary(_) => "binary",
            Value::Closure(_) => "closure",
            Value::List(items) => {
                if !items.is_empty() && items.iter().all(|x| matches!(x, Value::Record(_))) {
                    "table"
                } else {
                    "list"
                }
            }
            Value::Record(_) => "record",
        }
        .to_string()
    } else {
        "list".to_string()
    };
    Ok(PipelineData::Values(vec![Value::String(name)]))
}

// ---------------------------------------------------------------------------
// Phase 7d — path / date
// ---------------------------------------------------------------------------

fn vb_path(
    input: PipelineData,
    args: &[String],
    _state: &mut ShellState,
) -> Result<PipelineData, i32> {
    let sub = match args.first().map(|s| s.as_str()) {
        Some(s) => s,
        None => {
            eprintln!("path: usage `path <join|basename|dirname|parse|exists> ...`");
            return Err(1);
        }
    };
    let rest = &args[1..];
    // Helper: derive the input string(s) — either from rest args or from pipeline.
    let from_input_or_args =
        |input: PipelineData, fallback: &[String]| -> Result<Vec<String>, i32> {
            if !fallback.is_empty() {
                return Ok(fallback.to_vec());
            }
            let vs = input.into_values()?;
            Ok(vs.into_iter().map(|v| v.to_display_string()).collect())
        };
    match sub {
        "join" => {
            // `path join a b c` → "a/b/c"
            let pieces: Vec<String> = if !rest.is_empty() {
                rest.to_vec()
            } else {
                let vs = input.into_values()?;
                vs.into_iter().map(|v| v.to_display_string()).collect()
            };
            let mut p = std::path::PathBuf::new();
            for piece in &pieces {
                p.push(piece);
            }
            Ok(PipelineData::Values(vec![Value::String(
                p.to_string_lossy().to_string(),
            )]))
        }
        "basename" => {
            let inputs = from_input_or_args(input, rest)?;
            let out: Vec<Value> = inputs
                .into_iter()
                .map(|s| {
                    let p = std::path::Path::new(&s);
                    Value::String(
                        p.file_name()
                            .map(|n| n.to_string_lossy().to_string())
                            .unwrap_or_default(),
                    )
                })
                .collect();
            Ok(PipelineData::Values(out))
        }
        "dirname" => {
            let inputs = from_input_or_args(input, rest)?;
            let out: Vec<Value> = inputs
                .into_iter()
                .map(|s| {
                    let p = std::path::Path::new(&s);
                    Value::String(
                        p.parent()
                            .map(|n| n.to_string_lossy().to_string())
                            .unwrap_or_default(),
                    )
                })
                .collect();
            Ok(PipelineData::Values(out))
        }
        "exists" => {
            let inputs = from_input_or_args(input, rest)?;
            let out: Vec<Value> = inputs
                .into_iter()
                .map(|s| Value::Bool(std::path::Path::new(&s).exists()))
                .collect();
            Ok(PipelineData::Values(out))
        }
        "parse" => {
            let inputs = from_input_or_args(input, rest)?;
            let out: Vec<Value> = inputs
                .into_iter()
                .map(|s| {
                    let p = std::path::Path::new(&s);
                    let mut r = IndexMap::new();
                    r.insert(
                        "parent".to_string(),
                        Value::String(
                            p.parent()
                                .map(|n| n.to_string_lossy().to_string())
                                .unwrap_or_default(),
                        ),
                    );
                    r.insert(
                        "stem".to_string(),
                        Value::String(
                            p.file_stem()
                                .map(|n| n.to_string_lossy().to_string())
                                .unwrap_or_default(),
                        ),
                    );
                    r.insert(
                        "extension".to_string(),
                        Value::String(
                            p.extension()
                                .map(|n| n.to_string_lossy().to_string())
                                .unwrap_or_default(),
                        ),
                    );
                    Value::Record(r)
                })
                .collect();
            Ok(PipelineData::Values(out))
        }
        _ => {
            eprintln!("path: unknown subcommand '{}'", sub);
            Err(1)
        }
    }
}

fn vb_date(
    input: PipelineData,
    args: &[String],
    _state: &mut ShellState,
) -> Result<PipelineData, i32> {
    let sub = match args.first().map(|s| s.as_str()) {
        Some(s) => s,
        None => {
            eprintln!("date: usage `date now | date format <fmt>`");
            return Err(1);
        }
    };
    match sub {
        "now" => {
            // RFC3339 timestamp of "now" (UTC), no external chrono dependency.
            let now = std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map_err(|_| 1_i32)?;
            let secs = now.as_secs() as i64;
            let nanos = now.subsec_nanos();
            Ok(PipelineData::Values(vec![Value::String(format_rfc3339(
                secs, nanos,
            ))]))
        }
        "format" => {
            // `date format <fmt>` — accepts a few simple specifiers on UTC now or
            // on the input string (which must be an integer epoch second).
            let fmt = args
                .get(1)
                .map(|s| s.as_str())
                .unwrap_or("%Y-%m-%d %H:%M:%S");
            // Determine the source seconds: pipeline if non-empty, else now.
            let inputs = input.into_values().unwrap_or_default();
            let secs_list: Vec<i64> = if inputs.is_empty() {
                let now = std::time::SystemTime::now()
                    .duration_since(std::time::UNIX_EPOCH)
                    .map_err(|_| 1_i32)?;
                vec![now.as_secs() as i64]
            } else {
                inputs
                    .iter()
                    .filter_map(|v| match v {
                        Value::Int(i) => Some(*i),
                        Value::String(s) => s.parse().ok(),
                        _ => None,
                    })
                    .collect()
            };
            let out: Vec<Value> = secs_list
                .into_iter()
                .map(|s| Value::String(format_date(s, fmt)))
                .collect();
            Ok(PipelineData::Values(out))
        }
        _ => {
            eprintln!("date: unknown subcommand '{}'", sub);
            Err(1)
        }
    }
}

// Format seconds-since-epoch as RFC3339 (UTC), no external deps.
fn format_rfc3339(secs: i64, nanos: u32) -> String {
    let (y, mo, d, h, mi, se) = epoch_to_ymdhms(secs);
    format!(
        "{:04}-{:02}-{:02}T{:02}:{:02}:{:02}.{:09}Z",
        y, mo, d, h, mi, se, nanos
    )
}

fn format_date(secs: i64, fmt: &str) -> String {
    let (y, mo, d, h, mi, se) = epoch_to_ymdhms(secs);
    let mut out = String::with_capacity(fmt.len());
    let mut chars = fmt.chars().peekable();
    while let Some(c) = chars.next() {
        if c == '%' {
            match chars.next() {
                Some('Y') => out.push_str(&format!("{:04}", y)),
                Some('m') => out.push_str(&format!("{:02}", mo)),
                Some('d') => out.push_str(&format!("{:02}", d)),
                Some('H') => out.push_str(&format!("{:02}", h)),
                Some('M') => out.push_str(&format!("{:02}", mi)),
                Some('S') => out.push_str(&format!("{:02}", se)),
                Some('%') => out.push('%'),
                Some(other) => {
                    out.push('%');
                    out.push(other);
                }
                None => out.push('%'),
            }
        } else {
            out.push(c);
        }
    }
    out
}

// Civil-from-days algorithm (Howard Hinnant). secs is UTC seconds since epoch.
fn epoch_to_ymdhms(secs: i64) -> (i32, u32, u32, u32, u32, u32) {
    let days = secs.div_euclid(86_400);
    let secs_of_day = secs.rem_euclid(86_400);
    let h = (secs_of_day / 3600) as u32;
    let mi = ((secs_of_day % 3600) / 60) as u32;
    let se = (secs_of_day % 60) as u32;
    let z = days + 719_468;
    let era = if z >= 0 { z } else { z - 146_096 } / 146_097;
    let doe = (z - era * 146_097) as u64;
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let y = yoe as i64 + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = (doy - (153 * mp + 2) / 5 + 1) as u32;
    let m = (if mp < 10 { mp + 3 } else { mp - 9 }) as u32;
    let y = (if m <= 2 { y + 1 } else { y }) as i32;
    (y, m, d, h, mi, se)
}

// ---------------------------------------------------------------------------
// Phase 9b — format template
// ---------------------------------------------------------------------------

/// `format "{name}: {age}"` — for each record, replace each `{field}` token
/// (with optional dotted/indexed path) with that field's display value.
/// For a non-record value, `{}` (or `{$it}`) refers to the value itself.
fn vb_format(
    input: PipelineData,
    args: &[String],
    _state: &mut ShellState,
) -> Result<PipelineData, i32> {
    let tmpl = match args.first() {
        Some(s) => s.clone(),
        None => {
            eprintln!("format: missing template");
            return Err(1);
        }
    };
    let vs = input.into_values()?;
    let render = |v: &Value| -> String {
        let mut out = String::with_capacity(tmpl.len());
        let bytes = tmpl.as_bytes();
        let mut i = 0;
        while i < bytes.len() {
            if bytes[i] == b'{' {
                let mut j = i + 1;
                while j < bytes.len() && bytes[j] != b'}' {
                    j += 1;
                }
                if j >= bytes.len() {
                    out.push('{');
                    i += 1;
                    continue;
                }
                let key = std::str::from_utf8(&bytes[i + 1..j]).unwrap_or("");
                if key.is_empty() || key == "$it" {
                    out.push_str(&v.to_display_string());
                } else {
                    let path = parse_cell_path(key.trim_start_matches('$'));
                    let r = resolve_cell_path(v, &path).unwrap_or(Value::Null);
                    out.push_str(&r.to_display_string());
                }
                i = j + 1;
            } else {
                out.push(bytes[i] as char);
                i += 1;
            }
        }
        out
    };
    let out: Vec<Value> = vs.iter().map(|v| Value::String(render(v))).collect();
    Ok(PipelineData::Values(out))
}

// ---------------------------------------------------------------------------
// Phase 9c — do (execute closure inline)
// ---------------------------------------------------------------------------

fn vb_do(
    input: PipelineData,
    args: &[String],
    state: &mut ShellState,
) -> Result<PipelineData, i32> {
    let closure = match lookup_closure(args.first(), state) {
        Some(c) => c,
        None => {
            eprintln!("do: missing closure argument");
            return Err(1);
        }
    };
    // Closure positional args come from args[1..]. If no extra args are given
    // and the pipeline has input, pass the input as a single value.
    let mut call_args: Vec<Value> = args
        .iter()
        .skip(1)
        .map(|s| Value::String(s.clone()))
        .collect();
    if call_args.is_empty() {
        match input {
            PipelineData::Empty => {}
            PipelineData::Bytes(b) => {
                let s = String::from_utf8_lossy(&b)
                    .trim_end_matches('\n')
                    .to_string();
                if !s.is_empty() {
                    call_args.push(Value::String(s));
                }
            }
            PipelineData::Values(mut vs) => {
                if vs.len() == 1 {
                    call_args.push(vs.remove(0));
                } else if !vs.is_empty() {
                    call_args.push(Value::List(vs));
                }
            }
            PipelineData::Stream(it) => {
                let mut vs: Vec<Value> = it.collect();
                if vs.len() == 1 {
                    call_args.push(vs.remove(0));
                } else if !vs.is_empty() {
                    call_args.push(Value::List(vs));
                }
            }
        }
    }
    let result = crate::executor::apply_closure(&closure, &call_args, state)?;
    // Spread List results into pipeline values so downstream stages see them
    // element-wise (matches nushell's auto-spread).
    let out = match result {
        Value::List(vs) => PipelineData::Values(vs),
        other => PipelineData::Values(vec![other]),
    };
    Ok(out)
}

// ---------------------------------------------------------------------------
// Phase 10c — table utilities
// ---------------------------------------------------------------------------

/// `default <value> [field]` — replace Null with `value`. With `field`, only
/// touch that field on Record inputs; without it, apply to each pipeline
/// value (scalar replacement).
fn vb_default(
    input: PipelineData,
    args: &[String],
    _state: &mut ShellState,
) -> Result<PipelineData, i32> {
    let default_str = match args.first() {
        Some(s) => s.clone(),
        None => {
            eprintln!("default: missing value");
            return Err(1);
        }
    };
    let field = args.get(1).cloned();
    let default_val = coerce_string_to_value(&default_str);
    let vs = input.into_values()?;
    let mut out = Vec::with_capacity(vs.len());
    for v in vs {
        let new_v = match (&field, v) {
            (Some(f), Value::Record(mut rec)) => {
                let is_missing = rec.get(f).map(|x| matches!(x, Value::Null)).unwrap_or(true);
                if is_missing {
                    rec.insert(f.clone(), default_val.clone());
                }
                Value::Record(rec)
            }
            (None, Value::Null) => default_val.clone(),
            (_, other) => other,
        };
        out.push(new_v);
    }
    Ok(PipelineData::Values(out))
}

pub fn coerce_string_to_value(s: &str) -> Value {
    if let Ok(i) = s.parse::<i64>() {
        return Value::Int(i);
    }
    if let Ok(f) = s.parse::<f64>() {
        return Value::Float(f);
    }
    match s {
        "true" => Value::Bool(true),
        "false" => Value::Bool(false),
        "null" => Value::Null,
        _ => Value::String(s.to_string()),
    }
}

/// `transpose` — flip rows/columns. Input: list of records → list of
/// {column, row1, row2, ...} records. With positional names, those replace
/// the default "column"/"row0"/"row1"/... labels.
fn vb_transpose(
    input: PipelineData,
    args: &[String],
    _state: &mut ShellState,
) -> Result<PipelineData, i32> {
    let vs = input.into_values()?;
    // Collect column order from the first record; later records contribute
    // missing keys at the end.
    let mut cols: Vec<String> = Vec::new();
    let mut seen = std::collections::HashSet::new();
    for v in &vs {
        if let Value::Record(rec) = v {
            for k in rec.keys() {
                if seen.insert(k.clone()) {
                    cols.push(k.clone());
                }
            }
        }
    }
    if cols.is_empty() {
        return Ok(PipelineData::Values(Vec::new()));
    }
    let header_label = args
        .first()
        .cloned()
        .unwrap_or_else(|| "column".to_string());
    let n_rows = vs.len();
    let row_labels: Vec<String> = (0..n_rows)
        .map(|i| {
            args.get(i + 1)
                .cloned()
                .unwrap_or_else(|| format!("row{}", i))
        })
        .collect();
    let mut out = Vec::with_capacity(cols.len());
    for c in &cols {
        let mut rec = IndexMap::new();
        rec.insert(header_label.clone(), Value::String(c.clone()));
        for (i, v) in vs.iter().enumerate() {
            let cell = v.get(c).cloned().unwrap_or(Value::Null);
            rec.insert(row_labels[i].clone(), cell);
        }
        out.push(Value::Record(rec));
    }
    Ok(PipelineData::Values(out))
}

/// `shuffle` — random permutation of the input list.
fn vb_shuffle(
    input: PipelineData,
    _args: &[String],
    _state: &mut ShellState,
) -> Result<PipelineData, i32> {
    let mut vs = input.into_values()?;
    // Lightweight Fisher–Yates with a hash-of-time seed; no external rand
    // dep needed for what is essentially a convenience tool.
    use std::time::{SystemTime, UNIX_EPOCH};
    let seed = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_nanos())
        .unwrap_or(0) as u64;
    let mut state = seed ^ 0x9E3779B97F4A7C15u64;
    let mut next = || -> u64 {
        // @@KEEPXOJSHIFT@@64
        state ^= state << 13;
        state ^= state >> 7;
        state ^= state << 17;
        state
    };
    let n = vs.len();
    for i in (1..n).rev() {
        let j = (next() as usize) % (i + 1);
        vs.swap(i, j);
    }
    Ok(PipelineData::Values(vs))
}

// ---------------------------------------------------------------------------
// Phase 11a — sort / to-csv / chunks / window / split-by
// ---------------------------------------------------------------------------

fn compare_values(a: &Value, b: &Value) -> std::cmp::Ordering {
    use std::cmp::Ordering;
    // Numeric comparison when both look like numbers.
    if let (Some(x), Some(y)) = (a.as_f64(), b.as_f64()) {
        return x.partial_cmp(&y).unwrap_or(Ordering::Equal);
    }
    // Boolean: false < true.
    if let (Value::Bool(x), Value::Bool(y)) = (a, b) {
        return x.cmp(y);
    }
    // Null sorts before everything else.
    match (a, b) {
        (Value::Null, Value::Null) => Ordering::Equal,
        (Value::Null, _) => Ordering::Less,
        (_, Value::Null) => Ordering::Greater,
        _ => a.to_display_string().cmp(&b.to_display_string()),
    }
}

/// `sort [-r|--reverse]` — sort the pipeline values. Use `sort-by <field>`
/// for record-keyed sorting (already exists).
fn vb_sort(
    input: PipelineData,
    args: &[String],
    _state: &mut ShellState,
) -> Result<PipelineData, i32> {
    let reverse = args.iter().any(|a| a == "-r" || a == "--reverse");
    let mut vs = input.into_values()?;
    vs.sort_by(compare_values);
    if reverse {
        vs.reverse();
    }
    Ok(PipelineData::Values(vs))
}

/// `to-csv` — serialize a list of records to CSV bytes. Column order is
/// taken from the first record; later records contribute any new keys at
/// the end.
fn vb_to_csv(
    input: PipelineData,
    _args: &[String],
    _state: &mut ShellState,
) -> Result<PipelineData, i32> {
    let vs = input.into_values()?;
    let mut cols: Vec<String> = Vec::new();
    let mut seen = std::collections::HashSet::new();
    for v in &vs {
        if let Value::Record(rec) = v {
            for k in rec.keys() {
                if seen.insert(k.clone()) {
                    cols.push(k.clone());
                }
            }
        }
    }
    let escape = |s: &str| -> String {
        // RFC 4180: wrap in quotes if value contains comma, quote, or newline.
        if s.contains(',') || s.contains('"') || s.contains('\n') {
            format!("\"{}\"", s.replace('"', "\"\""))
        } else {
            s.to_string()
        }
    };
    let mut out = String::new();
    out.push_str(&cols.iter().map(|c| escape(c)).collect::<Vec<_>>().join(","));
    out.push('\n');
    for v in &vs {
        let row: Vec<String> = cols
            .iter()
            .map(|c| {
                let cell = v
                    .get(c)
                    .map(|fv| fv.to_display_string())
                    .unwrap_or_default();
                escape(&cell)
            })
            .collect();
        out.push_str(&row.join(","));
        out.push('\n');
    }
    Ok(PipelineData::Bytes(out.into_bytes()))
}

/// `chunks <size>` — split the pipeline list into fixed-size chunks. The
/// last chunk may be shorter.
fn vb_chunks(
    input: PipelineData,
    args: &[String],
    _state: &mut ShellState,
) -> Result<PipelineData, i32> {
    let size: usize = args.first().and_then(|s| s.parse().ok()).unwrap_or(0);
    if size == 0 {
        eprintln!("chunks: size must be a positive integer");
        return Err(1);
    }
    let vs = input.into_values()?;
    let out: Vec<Value> = vs.chunks(size).map(|c| Value::List(c.to_vec())).collect();
    Ok(PipelineData::Values(out))
}

/// `window <size> [--stride N]` — sliding window of `size` over the input.
/// Default stride is 1. The last window is dropped if it would be short.
fn vb_window(
    input: PipelineData,
    args: &[String],
    _state: &mut ShellState,
) -> Result<PipelineData, i32> {
    let mut size: usize = 0;
    let mut stride: usize = 1;
    let mut i = 0;
    while i < args.len() {
        match args[i].as_str() {
            "--stride" | "-s" => {
                i += 1;
                stride = args.get(i).and_then(|s| s.parse().ok()).unwrap_or(1);
            }
            other => {
                size = other.parse().unwrap_or(size);
            }
        }
        i += 1;
    }
    if size == 0 {
        eprintln!("window: size must be a positive integer");
        return Err(1);
    }
    if stride == 0 {
        stride = 1;
    }
    let vs = input.into_values()?;
    let mut out = Vec::new();
    let mut start = 0;
    while start + size <= vs.len() {
        out.push(Value::List(vs[start..start + size].to_vec()));
        start += stride;
    }
    Ok(PipelineData::Values(out))
}

/// `split-by <field>` — group records by `field` value, producing a Record
/// keyed by group values (vs. group-by which returns key/items records).
/// Useful when downstream needs `$grouped.<key>` access.
fn vb_split_by(
    input: PipelineData,
    args: &[String],
    _state: &mut ShellState,
) -> Result<PipelineData, i32> {
    let field = match args.first() {
        Some(f) => f,
        None => {
            eprintln!("split-by: missing field");
            return Err(1);
        }
    };
    let vs = input.into_values()?;
    let mut groups: IndexMap<String, Vec<Value>> = IndexMap::new();
    for v in vs {
        let key = v
            .get(field)
            .map(|fv| fv.to_display_string())
            .unwrap_or_default();
        groups.entry(key).or_default().push(v);
    }
    let mut rec = IndexMap::new();
    for (k, items) in groups {
        rec.insert(k, Value::List(items));
    }
    Ok(PipelineData::Values(vec![Value::Record(rec)]))
}

// ---------------------------------------------------------------------------
// Phase 11b — encode / decode (base64 / hex), hand-rolled, no extra deps
// ---------------------------------------------------------------------------

fn input_as_bytes(input: PipelineData) -> Vec<u8> {
    let input = match input {
        PipelineData::Stream(it) => PipelineData::Values(it.collect()),
        other => other,
    };
    match input {
        PipelineData::Empty => Vec::new(),
        PipelineData::Bytes(b) => {
            // Strip one trailing newline so `echo foo | encode hex` doesn't
            // include the implicit newline from echo. Matches the str-builtin
            // convention adopted in Phase 9b.
            if b.last() == Some(&b'\n') {
                b[..b.len() - 1].to_vec()
            } else {
                b
            }
        }
        PipelineData::Stream(_) => unreachable!("normalized above"),
        PipelineData::Values(vs) => {
            if vs.len() == 1 {
                match &vs[0] {
                    Value::String(s) => s.as_bytes().to_vec(),
                    Value::Binary(b) => b.clone(),
                    other => other.to_display_string().into_bytes(),
                }
            } else {
                vs.iter()
                    .map(|v| v.to_display_string())
                    .collect::<Vec<_>>()
                    .join("\n")
                    .into_bytes()
            }
        }
    }
}

const B64_TABLE: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";

fn b64_encode(input: &[u8]) -> String {
    let mut out = String::with_capacity(input.len().div_ceil(3) * 4);
    let mut chunks = input.chunks_exact(3);
    for c in chunks.by_ref() {
        let n = ((c[0] as u32) << 16) | ((c[1] as u32) << 8) | (c[2] as u32);
        out.push(B64_TABLE[((n >> 18) & 0x3F) as usize] as char);
        out.push(B64_TABLE[((n >> 12) & 0x3F) as usize] as char);
        out.push(B64_TABLE[((n >> 6) & 0x3F) as usize] as char);
        out.push(B64_TABLE[(n & 0x3F) as usize] as char);
    }
    let rem = chunks.remainder();
    match rem.len() {
        1 => {
            let n = (rem[0] as u32) << 16;
            out.push(B64_TABLE[((n >> 18) & 0x3F) as usize] as char);
            out.push(B64_TABLE[((n >> 12) & 0x3F) as usize] as char);
            out.push('=');
            out.push('=');
        }
        2 => {
            let n = ((rem[0] as u32) << 16) | ((rem[1] as u32) << 8);
            out.push(B64_TABLE[((n >> 18) & 0x3F) as usize] as char);
            out.push(B64_TABLE[((n >> 12) & 0x3F) as usize] as char);
            out.push(B64_TABLE[((n >> 6) & 0x3F) as usize] as char);
            out.push('=');
        }
        _ => {}
    }
    out
}

fn b64_decode(input: &str) -> Result<Vec<u8>, String> {
    let mut lut = [255u8; 256];
    for (i, &b) in B64_TABLE.iter().enumerate() {
        lut[b as usize] = i as u8;
    }
    let clean: Vec<u8> = input.bytes().filter(|b| !b.is_ascii_whitespace()).collect();
    // Strip padding chars; bit-accumulator naturally drops the partial byte.
    let trimmed: &[u8] = clean
        .strip_suffix(b"==")
        .or_else(|| clean.strip_suffix(b"="))
        .unwrap_or(&clean);
    let mut out = Vec::with_capacity(trimmed.len() / 4 * 3);
    let mut buf: u32 = 0;
    let mut bits = 0;
    for &b in trimmed {
        let v = lut[b as usize];
        if v == 255 {
            return Err(format!("invalid base64 char: {}", b as char));
        }
        buf = (buf << 6) | (v as u32);
        bits += 6;
        if bits >= 8 {
            bits -= 8;
            out.push(((buf >> bits) & 0xFF) as u8);
        }
    }
    Ok(out)
}

fn hex_encode(input: &[u8]) -> String {
    const HEX: &[u8; 16] = b"0123456789abcdef";
    let mut out = String::with_capacity(input.len() * 2);
    for &b in input {
        out.push(HEX[(b >> 4) as usize] as char);
        out.push(HEX[(b & 0xF) as usize] as char);
    }
    out
}

fn hex_decode(input: &str) -> Result<Vec<u8>, String> {
    let clean: Vec<u8> = input.bytes().filter(|b| !b.is_ascii_whitespace()).collect();
    if !clean.len().is_multiple_of(2) {
        return Err("odd number of hex digits".into());
    }
    let from_hex = |b: u8| -> Result<u8, String> {
        match b {
            b'0'..=b'9' => Ok(b - b'0'),
            b'a'..=b'f' => Ok(b - b'a' + 10),
            b'A'..=b'F' => Ok(b - b'A' + 10),
            _ => Err(format!("invalid hex char: {}", b as char)),
        }
    };
    let mut out = Vec::with_capacity(clean.len() / 2);
    for pair in clean.chunks_exact(2) {
        out.push((from_hex(pair[0])? << 4) | from_hex(pair[1])?);
    }
    Ok(out)
}

/// `encode <base64|hex>` — encode pipeline bytes to a string.
fn vb_encode(
    input: PipelineData,
    args: &[String],
    _state: &mut ShellState,
) -> Result<PipelineData, i32> {
    let scheme = match args.first().map(|s| s.as_str()) {
        Some(s) => s,
        None => {
            eprintln!("encode: missing scheme (base64|hex)");
            return Err(1);
        }
    };
    let bytes = input_as_bytes(input);
    let s = match scheme {
        "base64" | "b64" => b64_encode(&bytes),
        "hex" => hex_encode(&bytes),
        other => {
            eprintln!("encode: unknown scheme '{}'", other);
            return Err(1);
        }
    };
    Ok(PipelineData::Values(vec![Value::String(s)]))
}

/// `decode <base64|hex>` — decode a string into bytes. Output is a String
/// when the result is valid UTF-8, otherwise Binary.
fn vb_decode(
    input: PipelineData,
    args: &[String],
    _state: &mut ShellState,
) -> Result<PipelineData, i32> {
    let scheme = match args.first().map(|s| s.as_str()) {
        Some(s) => s,
        None => {
            eprintln!("decode: missing scheme (base64|hex)");
            return Err(1);
        }
    };
    let bytes = input_as_bytes(input);
    let s = String::from_utf8_lossy(&bytes).into_owned();
    let decoded = match scheme {
        "base64" | "b64" => b64_decode(&s).map_err(|e| {
            eprintln!("decode: {}", e);
            1
        })?,
        "hex" => hex_decode(&s).map_err(|e| {
            eprintln!("decode: {}", e);
            1
        })?,
        other => {
            eprintln!("decode: unknown scheme '{}'", other);
            return Err(1);
        }
    };
    let v = match String::from_utf8(decoded.clone()) {
        Ok(s) => Value::String(s),
        Err(_) => Value::Binary(decoded),
    };
    Ok(PipelineData::Values(vec![v]))
}

// ---------------------------------------------------------------------------
// Phase 12c — url parse / url join
// ---------------------------------------------------------------------------

/// `url parse [<str>]` — split a URL into a Record with `scheme`, `username`,
/// `password`, `host`, `port`, `path`, `query`, `fragment`, and `params`
/// (parsed query as a Record). If no positional arg is given, reads the URL
/// from pipeline input.
///
/// `url join <record>` — opposite: serialize a Record back to a URL string.
/// Missing/empty fields are omitted (no `:port`, no `?`, no `#`).
fn vb_url(
    input: PipelineData,
    args: &[String],
    _state: &mut ShellState,
) -> Result<PipelineData, i32> {
    let sub = match args.first().map(|s| s.as_str()) {
        Some(s) => s,
        None => {
            eprintln!("url: missing subcommand (parse|join)");
            return Err(1);
        }
    };
    let rest = &args[1..];
    match sub {
        "parse" => {
            let s = if let Some(arg) = rest.first() {
                arg.clone()
            } else {
                input_as_string(input)?.trim().to_string()
            };
            let rec = parse_url(&s);
            Ok(PipelineData::Values(vec![Value::Record(rec)]))
        }
        "join" => {
            let rec = if let Some(_arg) = rest.first() {
                // Allow `url join <json-record-string>` for ergonomic CLI use.
                let txt = rest[0].clone();
                match serde_json::from_str::<serde_json::Value>(&txt) {
                    Ok(j) => match Value::from_json(j) {
                        Value::Record(r) => r,
                        _ => {
                            eprintln!("url join: argument must be a Record");
                            return Err(1);
                        }
                    },
                    Err(_) => {
                        eprintln!("url join: argument must be a JSON Record");
                        return Err(1);
                    }
                }
            } else {
                let vs = input.into_values()?;
                match vs.into_iter().next() {
                    Some(Value::Record(r)) => r,
                    _ => {
                        eprintln!("url join: pipeline input must be a Record");
                        return Err(1);
                    }
                }
            };
            let s = join_url(&rec);
            Ok(PipelineData::Values(vec![Value::String(s)]))
        }
        other => {
            eprintln!("url: unknown subcommand '{}'", other);
            Err(1)
        }
    }
}

fn parse_url(input: &str) -> IndexMap<String, Value> {
    let mut rec: IndexMap<String, Value> = IndexMap::new();
    let mut rest = input;

    // scheme://
    let scheme = if let Some(idx) = rest.find("://") {
        let s = &rest[..idx];
        rest = &rest[idx + 3..];
        s.to_string()
    } else {
        String::new()
    };

    // fragment (split first so it's never confused with query)
    let fragment = if let Some(idx) = rest.find('#') {
        let f = rest[idx + 1..].to_string();
        rest = &rest[..idx];
        f
    } else {
        String::new()
    };

    // query
    let query = if let Some(idx) = rest.find('?') {
        let q = rest[idx + 1..].to_string();
        rest = &rest[..idx];
        q
    } else {
        String::new()
    };

    // path (everything from first '/')
    let (authority, path) = if let Some(idx) = rest.find('/') {
        (&rest[..idx], rest[idx..].to_string())
    } else {
        (rest, String::new())
    };

    // userinfo@host:port
    let (userinfo, hostport) = if let Some(idx) = authority.rfind('@') {
        (&authority[..idx], &authority[idx + 1..])
    } else {
        ("", authority)
    };
    let (username, password) = if userinfo.is_empty() {
        (String::new(), String::new())
    } else if let Some(idx) = userinfo.find(':') {
        (userinfo[..idx].to_string(), userinfo[idx + 1..].to_string())
    } else {
        (userinfo.to_string(), String::new())
    };
    // host:port — handle IPv6 `[::1]:8080` as a special case.
    let (host, port) = if let Some(stripped) = hostport.strip_prefix('[') {
        if let Some(end) = stripped.find(']') {
            let h = format!("[{}]", &stripped[..end]);
            let after = &stripped[end + 1..];
            let p = after.strip_prefix(':').unwrap_or("").to_string();
            (h, p)
        } else {
            (hostport.to_string(), String::new())
        }
    } else if let Some(idx) = hostport.rfind(':') {
        (hostport[..idx].to_string(), hostport[idx + 1..].to_string())
    } else {
        (hostport.to_string(), String::new())
    };

    // params = parsed query
    let mut params: IndexMap<String, Value> = IndexMap::new();
    if !query.is_empty() {
        for kv in query.split('&') {
            if kv.is_empty() {
                continue;
            }
            let (k, v) = match kv.find('=') {
                Some(i) => (&kv[..i], &kv[i + 1..]),
                None => (kv, ""),
            };
            params.insert(k.to_string(), Value::String(v.to_string()));
        }
    }

    rec.insert("scheme".to_string(), Value::String(scheme));
    rec.insert("username".to_string(), Value::String(username));
    rec.insert("password".to_string(), Value::String(password));
    rec.insert("host".to_string(), Value::String(host));
    rec.insert("port".to_string(), Value::String(port));
    rec.insert("path".to_string(), Value::String(path));
    rec.insert("query".to_string(), Value::String(query));
    rec.insert("fragment".to_string(), Value::String(fragment));
    rec.insert("params".to_string(), Value::Record(params));
    rec
}

fn join_url(rec: &IndexMap<String, Value>) -> String {
    let get = |k: &str| -> String {
        match rec.get(k) {
            Some(Value::String(s)) => s.clone(),
            Some(Value::Null) | None => String::new(),
            Some(v) => v.to_display_string(),
        }
    };
    let scheme = get("scheme");
    let username = get("username");
    let password = get("password");
    let host = get("host");
    let port = get("port");
    let path = get("path");
    let fragment = get("fragment");
    // Prefer params (Record) when present, else fall back to literal query.
    let query = match rec.get("params") {
        Some(Value::Record(p)) if !p.is_empty() => p
            .iter()
            .map(|(k, v)| format!("{}={}", k, v.to_display_string()))
            .collect::<Vec<_>>()
            .join("&"),
        _ => get("query"),
    };

    let mut out = String::new();
    if !scheme.is_empty() {
        out.push_str(&scheme);
        out.push_str("://");
    }
    if !username.is_empty() {
        out.push_str(&username);
        if !password.is_empty() {
            out.push(':');
            out.push_str(&password);
        }
        out.push('@');
    }
    out.push_str(&host);
    if !port.is_empty() {
        out.push(':');
        out.push_str(&port);
    }
    if !path.is_empty() {
        out.push_str(&path);
    }
    if !query.is_empty() {
        out.push('?');
        out.push_str(&query);
    }
    if !fragment.is_empty() {
        out.push('#');
        out.push_str(&fragment);
    }
    out
}

// ---------------------------------------------------------------------------
// Phase 13a — list utilities (prepend, append, drop, headers)
// ---------------------------------------------------------------------------

/// `prepend <val>` — push a single value to the front of the pipeline list.
/// `<val>` is coerced via `coerce_string_to_value` (int / float / bool / null /
/// string), or accepted as a JSON literal for structured values.
fn vb_prepend(
    input: PipelineData,
    args: &[String],
    _state: &mut ShellState,
) -> Result<PipelineData, i32> {
    let v = arg_to_value(args)?;
    let mut vs = input.into_values()?;
    vs.insert(0, v);
    Ok(PipelineData::Values(vs))
}

/// `append <val>` — push a single value to the back of the pipeline list.
fn vb_append(
    input: PipelineData,
    args: &[String],
    _state: &mut ShellState,
) -> Result<PipelineData, i32> {
    let v = arg_to_value(args)?;
    let mut vs = input.into_values()?;
    vs.push(v);
    Ok(PipelineData::Values(vs))
}

fn arg_to_value(args: &[String]) -> Result<Value, i32> {
    let a = match args.first() {
        Some(a) => a,
        None => {
            eprintln!("expected a value argument");
            return Err(1);
        }
    };
    // Try JSON first (so `[1,2,3]` / `{"a":1}` work); fall back to coercion.
    if let Ok(j) = serde_json::from_str::<serde_json::Value>(a) {
        return Ok(Value::from_json(j));
    }
    Ok(coerce_string_to_value(a))
}

/// `drop [n]` — drop the last `n` rows (default 1). Complementary to `last n`.
fn vb_drop(
    input: PipelineData,
    args: &[String],
    _state: &mut ShellState,
) -> Result<PipelineData, i32> {
    let n: usize = args.first().and_then(|s| s.parse().ok()).unwrap_or(1);
    let mut vs = input.into_values()?;
    let keep = vs.len().saturating_sub(n);
    vs.truncate(keep);
    Ok(PipelineData::Values(vs))
}

/// `headers` — interpret a list-of-lists as a table with the first row as
/// column headers, producing a list of records. Trailing rows that are shorter
/// than the header row get Null in the missing columns.
fn vb_headers(
    input: PipelineData,
    _args: &[String],
    _state: &mut ShellState,
) -> Result<PipelineData, i32> {
    let vs = input.into_values()?;
    if vs.is_empty() {
        return Ok(PipelineData::Values(vec![]));
    }
    let header_row = match &vs[0] {
        Value::List(xs) => xs.iter().map(|v| v.to_display_string()).collect::<Vec<_>>(),
        _ => {
            eprintln!("headers: first row must be a list of column names");
            return Err(1);
        }
    };
    let mut out = Vec::with_capacity(vs.len().saturating_sub(1));
    for row in vs.into_iter().skip(1) {
        let cells: Vec<Value> = match row {
            Value::List(xs) => xs,
            other => vec![other],
        };
        let mut rec: IndexMap<String, Value> = IndexMap::new();
        for (i, name) in header_row.iter().enumerate() {
            rec.insert(name.clone(), cells.get(i).cloned().unwrap_or(Value::Null));
        }
        out.push(Value::Record(rec));
    }
    Ok(PipelineData::Values(out))
}

// ---------------------------------------------------------------------------
// Phase 13b — histogram + Levenshtein
// ---------------------------------------------------------------------------

/// `histogram [field]` — like `unique -c` but adds a `freq` ratio and a
/// rendered ASCII `bar`. With `field`, counts grouped by `<row>.<field>`.
fn vb_histogram(
    input: PipelineData,
    args: &[String],
    _state: &mut ShellState,
) -> Result<PipelineData, i32> {
    let field = args.first().cloned();
    let vs = input.into_values()?;
    let total = vs.len();
    if total == 0 {
        return Ok(PipelineData::Values(vec![]));
    }
    let mut buckets: IndexMap<String, (Value, usize)> = IndexMap::new();
    for v in vs {
        let key_val = match &field {
            Some(f) => v.get(f).cloned().unwrap_or(Value::Null),
            None => v.clone(),
        };
        let key = key_val.to_display_string();
        let entry = buckets.entry(key).or_insert((key_val, 0));
        entry.1 += 1;
    }
    let mut rows: Vec<(Value, usize)> = buckets.into_iter().map(|(_, v)| v).collect();
    rows.sort_by_key(|row| std::cmp::Reverse(row.1));
    let max = rows.iter().map(|r| r.1).max().unwrap_or(1).max(1);
    let out: Vec<Value> = rows
        .into_iter()
        .map(|(val, count)| {
            let freq = count as f64 / total as f64;
            let bar_len = ((count as f64 / max as f64) * 30.0).round() as usize;
            let bar: String = std::iter::repeat_n('*', bar_len).collect();
            let mut rec: IndexMap<String, Value> = IndexMap::new();
            rec.insert("value".to_string(), val);
            rec.insert("count".to_string(), Value::Int(count as i64));
            rec.insert("freq".to_string(), Value::Float(freq));
            rec.insert("bar".to_string(), Value::String(bar));
            Value::Record(rec)
        })
        .collect();
    Ok(PipelineData::Values(out))
}

/// Classic Levenshtein distance, char-level (no allocation per cell beyond the
/// two rolling rows). Used by `str distance`.
/// Phase 15d: return the best did-you-mean candidate (edit distance ≤ 2 and
/// strictly smaller than the typed length) from a list of names.
pub fn closest_match<'a, I>(typed: &str, candidates: I) -> Option<String>
where
    I: IntoIterator<Item = &'a str>,
{
    let mut best: Option<(String, usize)> = None;
    for c in candidates {
        if c.is_empty() {
            continue;
        }
        let d = levenshtein(typed, c);
        if d == 0 || d > 2 || d >= typed.len() {
            continue;
        }
        match &best {
            Some((_, bd)) if d < *bd => best = Some((c.to_string(), d)),
            None => best = Some((c.to_string(), d)),
            _ => {}
        }
    }
    best.map(|(s, _)| s)
}

/// Collect candidate field/column names from a typical pipeline-input value
/// (single record, list of records, or single list-element record).
fn record_keys_from_input(vs: &[Value]) -> Vec<String> {
    let mut keys: Vec<String> = Vec::new();
    for v in vs {
        if let Value::Record(r) = v {
            for k in r.keys() {
                if !keys.iter().any(|x| x == k) {
                    keys.push(k.clone());
                }
            }
        }
    }
    keys
}

fn levenshtein(a: &str, b: &str) -> usize {
    let av: Vec<char> = a.chars().collect();
    let bv: Vec<char> = b.chars().collect();
    if av.is_empty() {
        return bv.len();
    }
    if bv.is_empty() {
        return av.len();
    }
    let mut prev: Vec<usize> = (0..=bv.len()).collect();
    let mut cur: Vec<usize> = vec![0; bv.len() + 1];
    for i in 1..=av.len() {
        cur[0] = i;
        for j in 1..=bv.len() {
            let cost = if av[i - 1] == bv[j - 1] { 0 } else { 1 };
            cur[j] = (prev[j] + 1).min(cur[j - 1] + 1).min(prev[j - 1] + cost);
        }
        std::mem::swap(&mut prev, &mut cur);
    }
    prev[bv.len()]
}

// ---------------------------------------------------------------------------
// Phase 13c — char / ansi / fill
// ---------------------------------------------------------------------------

/// `char <name>` — emit a single character or short escape sequence by name.
/// Aligns with nushell's `char` for common cases.
fn vb_char(
    _input: PipelineData,
    args: &[String],
    _state: &mut ShellState,
) -> Result<PipelineData, i32> {
    let name = match args.first() {
        Some(s) => s.as_str(),
        None => {
            eprintln!("char: missing name");
            return Err(1);
        }
    };
    let s = match name {
        "newline" | "nl" | "lf" => "\n".to_string(),
        "tab" => "\t".to_string(),
        "cr" => "\r".to_string(),
        "crlf" => "\r\n".to_string(),
        "space" | "sp" => " ".to_string(),
        "null" | "nul" => "\0".to_string(),
        "esc" | "escape" => "\x1b".to_string(),
        "bel" => "\x07".to_string(),
        "backspace" | "bs" => "\x08".to_string(),
        // Hex codepoint: `char 0x41` → "A". Also accepts plain `41` if it
        // parses as hex but not decimal-only — keep it strict for safety.
        s if s.starts_with("0x") || s.starts_with("0X") => {
            match u32::from_str_radix(&s[2..], 16)
                .ok()
                .and_then(char::from_u32)
            {
                Some(c) => c.to_string(),
                None => {
                    eprintln!("char: bad hex codepoint '{}'", s);
                    return Err(1);
                }
            }
        }
        // Single-char passthrough.
        s if s.chars().count() == 1 => s.to_string(),
        other => {
            eprintln!("char: unknown name '{}'", other);
            return Err(1);
        }
    };
    Ok(PipelineData::Values(vec![Value::String(s)]))
}

/// `ansi <code>` — emit an ANSI escape sequence. Names cover the common
/// colors plus `reset`. Use `ansi 0` for plain reset.
fn vb_ansi(
    _input: PipelineData,
    args: &[String],
    _state: &mut ShellState,
) -> Result<PipelineData, i32> {
    let name = match args.first() {
        Some(s) => s.as_str(),
        None => {
            eprintln!("ansi: missing color/code");
            return Err(1);
        }
    };
    let code = match name {
        "reset" | "rst" => "0",
        "bold" => "1",
        "dim" => "2",
        "italic" => "3",
        "underline" | "u" => "4",
        "black" => "30",
        "red" | "r" => "31",
        "green" | "g" => "32",
        "yellow" | "y" => "33",
        "blue" | "b" => "34",
        "magenta" | "m" | "purple" => "35",
        "cyan" | "c" => "36",
        "white" | "w" => "37",
        "bg_black" => "40",
        "bg_red" => "41",
        "bg_green" => "42",
        "bg_yellow" => "43",
        "bg_blue" => "44",
        "bg_magenta" => "45",
        "bg_cyan" => "46",
        "bg_white" => "47",
        // Numeric passthrough: `ansi 91` → bright red.
        s if s.bytes().all(|b| b.is_ascii_digit() || b == b';') => s,
        other => {
            eprintln!("ansi: unknown code '{}'", other);
            return Err(1);
        }
    };
    Ok(PipelineData::Values(vec![Value::String(format!(
        "\x1b[{}m",
        code
    ))]))
}

/// `fill -c <char> -w <width> [-a left|right|center]` — pad each input value
/// to `width` using `char` (default space). Alignment defaults to `left`
/// (pad on the right, matches nushell).
fn vb_fill(
    input: PipelineData,
    args: &[String],
    _state: &mut ShellState,
) -> Result<PipelineData, i32> {
    let mut ch = ' ';
    let mut width: usize = 0;
    let mut align = "left".to_string();
    let mut i = 0;
    while i < args.len() {
        match args[i].as_str() {
            "-c" | "--character" => {
                if let Some(v) = args.get(i + 1) {
                    ch = v.chars().next().unwrap_or(' ');
                    i += 2;
                } else {
                    i += 1;
                }
            }
            "-w" | "--width" => {
                if let Some(v) = args.get(i + 1) {
                    width = v.parse().unwrap_or(0);
                    i += 2;
                } else {
                    i += 1;
                }
            }
            "-a" | "--alignment" => {
                if let Some(v) = args.get(i + 1) {
                    align = v.clone();
                    i += 2;
                } else {
                    i += 1;
                }
            }
            _ => i += 1,
        }
    }
    let pad_one = |s: &str| -> String {
        let n = s.chars().count();
        if n >= width {
            return s.to_string();
        }
        let need = width - n;
        match align.as_str() {
            "right" | "r" => {
                let pad: String = std::iter::repeat_n(ch, need).collect();
                format!("{}{}", pad, s)
            }
            "center" | "c" => {
                let l = need / 2;
                let r = need - l;
                let lp: String = std::iter::repeat_n(ch, l).collect();
                let rp: String = std::iter::repeat_n(ch, r).collect();
                format!("{}{}{}", lp, s, rp)
            }
            _ => {
                let pad: String = std::iter::repeat_n(ch, need).collect();
                format!("{}{}", s, pad)
            }
        }
    };
    match input {
        PipelineData::Empty => Ok(PipelineData::Empty),
        PipelineData::Bytes(b) => {
            let s = String::from_utf8_lossy(&b);
            let trimmed = s.strip_suffix('\n').unwrap_or(&s);
            Ok(PipelineData::Values(vec![Value::String(pad_one(trimmed))]))
        }
        PipelineData::Values(vs) => {
            let out: Vec<Value> = vs
                .into_iter()
                .map(|v| Value::String(pad_one(&v.to_display_string())))
                .collect();
            Ok(PipelineData::Values(out))
        }
        PipelineData::Stream(it) => {
            let out: Vec<Value> = it
                .map(|v| Value::String(pad_one(&v.to_display_string())))
                .collect();
            Ok(PipelineData::Values(out))
        }
    }
}

// ---------------------------------------------------------------------------
// Phase 14a — structured errors + try/catch
// ---------------------------------------------------------------------------

/// Convert a pipeline input to a single Value for closure-arg passing.
/// Mirrors the convention used by `vb_do`: single value passes through,
/// multiple are wrapped in a List, bytes become a trimmed string.
fn input_to_value(input: PipelineData) -> Value {
    match input {
        PipelineData::Empty => Value::Null,
        PipelineData::Bytes(b) => {
            let s = String::from_utf8_lossy(&b);
            Value::String(s.trim_end_matches('\n').to_string())
        }
        PipelineData::Values(mut vs) => {
            if vs.is_empty() {
                Value::Null
            } else if vs.len() == 1 {
                vs.remove(0)
            } else {
                Value::List(vs)
            }
        }
        PipelineData::Stream(it) => {
            let mut vs: Vec<Value> = it.collect();
            if vs.is_empty() {
                Value::Null
            } else if vs.len() == 1 {
                vs.remove(0)
            } else {
                Value::List(vs)
            }
        }
    }
}

/// Wrap a closure-result Value back into PipelineData. List results are spread
/// so downstream stages iterate over them (matches nushell's auto-spread).
fn value_to_pipeline(v: Value) -> PipelineData {
    match v {
        Value::Null => PipelineData::Values(vec![Value::Null]),
        Value::List(vs) => PipelineData::Values(vs),
        other => PipelineData::Values(vec![other]),
    }
}

/// `try {|in| ... } [catch {|e| ... }]`
///
/// Runs the first closure with the pipeline input as its single positional
/// argument. On success, returns the closure's value. On error, consumes
/// `state.last_error` (or synthesizes a generic record if none was set) and
/// either invokes the catch closure with it, or — if no catch — returns Null.
fn vb_try(
    input: PipelineData,
    args: &[String],
    state: &mut ShellState,
) -> Result<PipelineData, i32> {
    let try_closure = match lookup_closure(args.first(), state) {
        Some(c) => c,
        None => {
            eprintln!("Usage: try {{|in| ...}} [catch {{|e| ...}}]");
            return Err(1);
        }
    };
    let catch_closure = if args.len() >= 3 && args[1] == "catch" {
        match lookup_closure(args.get(2), state) {
            Some(c) => Some(c),
            None => {
                eprintln!("try: `catch` requires a closure argument");
                return Err(1);
            }
        }
    } else {
        None
    };

    let in_val = input_to_value(input);
    let call_args: Vec<Value> = if matches!(in_val, Value::Null) {
        Vec::new()
    } else {
        vec![in_val]
    };

    // Clear any stale error so the try block starts from a known state.
    state.last_error = None;
    let saved_exit = state.last_exit_code;

    match crate::executor::apply_closure(&try_closure, &call_args, state) {
        Ok(v) => {
            state.last_error = None;
            Ok(value_to_pipeline(v))
        }
        Err(code) => {
            let err_rec = state.take_error().unwrap_or_else(|| {
                let mut rec = indexmap::IndexMap::new();
                rec.insert(
                    "msg".to_string(),
                    Value::String(format!("exit code {}", code)),
                );
                rec.insert("code".to_string(), Value::Int(code as i64));
                Value::Record(rec)
            });
            // Errors inside `try` are recovered: don't poison the outer exit code.
            state.last_exit_code = saved_exit;
            match catch_closure {
                Some(c) => {
                    let r = crate::executor::apply_closure(&c, &[err_rec], state)?;
                    Ok(value_to_pipeline(r))
                }
                None => Ok(PipelineData::Values(vec![Value::Null])),
            }
        }
    }
}

/// `error make <msg>` — raise a structured error with the given message.
/// The error is stored in `state.last_error` so an enclosing `try` can catch
/// it; the builtin itself returns Err(1) to interrupt the pipeline.
fn vb_error(
    _input: PipelineData,
    args: &[String],
    state: &mut ShellState,
) -> Result<PipelineData, i32> {
    if args.first().map(|s| s.as_str()) != Some("make") {
        eprintln!("Usage: error make <msg>");
        return Err(1);
    }
    let msg = args.get(1).cloned().unwrap_or_else(|| "error".to_string());
    state.set_error(msg, 1);
    Err(1)
}

// ---------------------------------------------------------------------------
// Phase 16a — module system: `use ./path.jsh [names...]`
// ---------------------------------------------------------------------------

/// `use PATH [NAME ...]`
///
/// Reads PATH as a jsh source file and executes it in the current shell, so
/// any `def`-registered functions land in `state.user_signatures` /
/// `state.user_typed_fns`. If extra NAMEs are supplied, only those newly
/// defined functions are kept; the rest are rolled back.
fn vb_use(
    _input: PipelineData,
    args: &[String],
    state: &mut ShellState,
) -> Result<PipelineData, i32> {
    let path = match args.first() {
        Some(p) => p,
        None => {
            eprintln!("Usage: use PATH [name ...]");
            return Err(1);
        }
    };
    let content = match crate::io_guard::read_regular_text_following(
        std::path::Path::new(path),
        MAX_MODULE_FILE_BYTES,
    ) {
        Ok(s) => s,
        Err(e) => {
            let msg = format!("use: cannot read `{}`: {}", path, e);
            eprintln!("{}", msg);
            state.set_error(msg, 1);
            return Err(1);
        }
    };
    let parsed = match crate::parser::parse(&content) {
        Ok(p) => p,
        Err(e) => {
            let msg = format!("use: parse error in `{}`: {:?}", path, e);
            eprintln!("{}", msg);
            state.set_error(msg, 1);
            return Err(1);
        }
    };
    let before: std::collections::HashSet<String> = state.user_signatures.keys().cloned().collect();
    for c in &parsed {
        let _ = crate::executor::execute_complete_command(c, state);
    }
    let added: Vec<String> = state
        .user_signatures
        .keys()
        .filter(|k| !before.contains(k.as_str()))
        .cloned()
        .collect();
    if args.len() > 1 {
        let keep: std::collections::HashSet<&str> = args[1..].iter().map(|s| s.as_str()).collect();
        for name in &added {
            if !keep.contains(name.as_str()) {
                state.user_signatures.remove(name);
                state.user_typed_fns.remove(name);
            }
        }
    }
    Ok(PipelineData::Empty)
}

// ---------------------------------------------------------------------------
// Phase 15c — typed user functions: `def NAME [a:int b:string ...] { ... }`
// ---------------------------------------------------------------------------

/// `def NAME [PARAM[:TYPE] ...] { |params| body }`
///
/// Registers a typed user function. The closure body is stored verbatim and
/// invoked through `apply_closure`. The parameter list is captured as a
/// `RuntimeSignature` so calls validate arity, `help NAME` works, completion
/// surfaces it, and the highlighter recognises it as a known command.
fn vb_def(
    _input: PipelineData,
    args: &[String],
    state: &mut ShellState,
) -> Result<PipelineData, i32> {
    if args.is_empty() {
        eprintln!("Usage: def NAME [params...] {{ |params| body }}");
        return Err(1);
    }
    let name = args[0].clone();
    if name.is_empty() {
        eprintln!("def: function name must be non-empty");
        return Err(1);
    }

    // The closure is the last argument (sentinel). Everything between
    // the name and that closure is a parameter declaration.
    let last_idx = args.len() - 1;
    let closure = match lookup_closure(args.get(last_idx), state) {
        Some(c) => c,
        None => {
            eprintln!("def {}: expected a closure body as the last argument", name);
            return Err(1);
        }
    };

    let mut params: Vec<crate::signature::RuntimeParam> = Vec::new();
    for raw in &args[1..last_idx] {
        // Allow trailing commas / brackets in param lists, mostly for ergonomic
        // forms like `def f [a:int, b:string] { ... }`. We strip cosmetic chars.
        let cleaned = raw.trim_matches(|c: char| c == ',' || c == '[' || c == ']');
        if cleaned.is_empty() {
            continue;
        }
        match crate::signature::parse_def_param(cleaned) {
            Some(p) => params.push(p),
            None => {
                eprintln!("def {}: malformed param `{}`", name, raw);
                return Err(1);
            }
        }
    }

    let sig = crate::signature::RuntimeSignature {
        name: name.clone(),
        params,
        desc: String::new(),
    };
    state.user_signatures.insert(name.clone(), sig);
    state.user_typed_fns.insert(name, closure);
    Ok(PipelineData::Empty)
}

// ---------------------------------------------------------------------------
// Phase 14b — `help <cmd>` (signature-driven)
// ---------------------------------------------------------------------------

/// `help <cmd>` — print the human-readable signature, or with `-r` return
/// the signature as a Record so it can flow through a pipeline.
fn vb_help(
    _input: PipelineData,
    args: &[String],
    state: &mut ShellState,
) -> Result<PipelineData, i32> {
    let mut as_record = false;
    let mut cmd: Option<&str> = None;
    for a in args {
        match a.as_str() {
            "-r" | "--record" => as_record = true,
            other => {
                if cmd.is_none() {
                    cmd = Some(other);
                }
            }
        }
    }
    let name = match cmd {
        Some(n) => n,
        None => {
            // No command given — list every catalog command, including those
            // whose help is summary-only, plus user-defined functions.
            let mut names: Vec<String> = crate::command_catalog::all_names()
                .iter()
                .map(|name| (*name).to_string())
                .chain(state.user_signatures.keys().cloned())
                .collect();
            names.sort();
            names.dedup();
            let out: Vec<Value> = names.into_iter().map(Value::String).collect();
            return Ok(PipelineData::Values(out));
        }
    };
    // User signatures shadow static ones — `def` lets users redefine semantics.
    if let Some(rsig) = state.user_signatures.get(name) {
        if as_record {
            return Ok(PipelineData::Values(vec![rsig.to_record()]));
        }
        return Ok(PipelineData::Bytes(rsig.render_help().into_bytes()));
    }
    if let Some(command) = crate::command_catalog::get(name) {
        if as_record {
            Ok(PipelineData::Values(vec![command.help_record()]))
        } else if let Some(signature) = command.signature() {
            let mut help = signature.render_help();
            if command.name != command.canonical_name {
                help.push_str(&format!(
                    "Alias: {} -> {}\n",
                    command.name, command.canonical_name
                ));
            }
            Ok(PipelineData::Bytes(help.into_bytes()))
        } else {
            let alias = if command.name == command.canonical_name {
                String::new()
            } else {
                format!(" (alias of {})", command.canonical_name)
            };
            let usage = command
                .usage()
                .map(|usage| format!("\nUsage: {usage}\n"))
                .unwrap_or_else(|| "\n".to_string());
            let detail = command
                .detail()
                .map(|detail| format!("\n{detail}\n"))
                .unwrap_or_default();
            Ok(PipelineData::Bytes(
                format!(
                    "{}{}\n  {}\n{}{}",
                    command.name,
                    alias,
                    command.summary(),
                    usage,
                    detail
                )
                .into_bytes(),
            ))
        }
    } else {
        state.set_error(format!("no signature for `{}`", name), 1);
        Err(1)
    }
}

// ---------------------------------------------------------------------------
// Phase 16c — http get/post
// ---------------------------------------------------------------------------

/// `http get URL [-H "K: V"] [--query "k=v"]`
/// `http post URL [-d body] [--json] [-H ...]`
///
/// Returns a record `{status: int, headers: record, body: string|value}`. If
/// the response content type is JSON the body is auto-parsed into a Value.
/// Append `--query` pairs to a URL, percent-encoding anything outside the
/// unreserved set. ureq 3 drives requests through `http::Uri`, which has no
/// query builder of its own, so the encoding lives here.
#[cfg(feature = "ai")]
fn append_query(url: &str, query: &[(String, String)]) -> String {
    let mut out = url.to_string();
    for (idx, (k, v)) in query.iter().enumerate() {
        let sep = if idx == 0 && !url.contains('?') {
            '?'
        } else {
            '&'
        };
        out.push(sep);
        push_query_encoded(&mut out, k);
        out.push('=');
        push_query_encoded(&mut out, v);
    }
    out
}

#[cfg(feature = "ai")]
fn push_query_encoded(out: &mut String, value: &str) {
    for byte in value.bytes() {
        match byte {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'.' | b'_' | b'~' => {
                out.push(byte as char)
            }
            _ => out.push_str(&format!("%{:02X}", byte)),
        }
    }
}

/// Serialize typed pipeline input incrementally so an infinite stream or one
/// with a very large prefix cannot grow an HTTP request body without bound.
#[cfg(feature = "ai")]
fn bounded_http_values(values: impl Iterator<Item = Value>) -> Result<Option<Vec<u8>>, String> {
    bounded_http_values_with_limit(values, MAX_HTTP_BODY_BYTES)
}

#[cfg(feature = "ai")]
fn bounded_http_values_with_limit(
    values: impl Iterator<Item = Value>,
    limit: usize,
) -> Result<Option<Vec<u8>>, String> {
    struct BoundedBody {
        bytes: Vec<u8>,
        limit: usize,
    }

    impl std::io::Write for BoundedBody {
        fn write(&mut self, bytes: &[u8]) -> std::io::Result<usize> {
            if self.bytes.len().saturating_add(bytes.len()) > self.limit {
                return Err(std::io::Error::new(
                    std::io::ErrorKind::InvalidData,
                    "request body byte limit exceeded",
                ));
            }
            self.bytes.extend_from_slice(bytes);
            Ok(bytes.len())
        }

        fn flush(&mut self) -> std::io::Result<()> {
            Ok(())
        }
    }

    let mut body = BoundedBody {
        bytes: Vec::new(),
        limit,
    };
    let mut count = 0usize;
    std::io::Write::write_all(&mut body, b"[").map_err(|error| error.to_string())?;
    for value in values {
        if count != 0 {
            std::io::Write::write_all(&mut body, b",").map_err(|error| error.to_string())?;
        }
        serde_json::to_writer(&mut body, &value.to_json()).map_err(|error| error.to_string())?;
        count = count.saturating_add(1);
    }
    if count == 0 {
        return Ok(None);
    }
    std::io::Write::write_all(&mut body, b"]\n").map_err(|error| error.to_string())?;
    Ok(Some(body.bytes))
}

#[cfg(feature = "ai")]
fn vb_http(
    input: PipelineData,
    args: &[String],
    state: &mut ShellState,
) -> Result<PipelineData, i32> {
    let method = match args.first().map(|s| s.as_str()) {
        Some("get") => "GET",
        Some("post") => "POST",
        Some("put") => "PUT",
        Some("delete") => "DELETE",
        Some(other) => {
            let msg = format!("http: unknown method `{}`", other);
            eprintln!("{}", msg);
            state.set_error(msg, 1);
            return Err(1);
        }
        None => {
            eprintln!("Usage: http get|post|put|delete URL [-H \"K: V\"] [-d body] [--json]");
            return Err(1);
        }
    };
    let url = match args.get(1) {
        Some(u) => u.clone(),
        None => {
            eprintln!("http: missing URL");
            return Err(1);
        }
    };
    if url.len() > MAX_HTTP_URL_BYTES || url.chars().any(char::is_control) {
        let msg = "http: URL contains a control character or exceeds its byte limit".to_string();
        eprintln!("{}", msg);
        state.set_error(msg, 1);
        return Err(1);
    }

    let mut headers: Vec<(String, String)> = Vec::new();
    let mut body_arg: Option<String> = None;
    let mut as_json = false;
    let mut query: Vec<(String, String)> = Vec::new();
    let mut i = 2;
    while i < args.len() {
        match args[i].as_str() {
            "-H" | "--header" => {
                if let Some(h) = args.get(i + 1) {
                    if let Some((k, v)) = h.split_once(':') {
                        headers.push((k.trim().to_string(), v.trim().to_string()));
                    }
                    i += 1;
                }
            }
            "-d" | "--data" => {
                body_arg = args.get(i + 1).cloned();
                i += 1;
            }
            "--json" => {
                as_json = true;
            }
            "--query" => {
                if let Some(q) = args.get(i + 1) {
                    if let Some((k, v)) = q.split_once('=') {
                        query.push((k.to_string(), v.to_string()));
                    }
                    i += 1;
                }
            }
            _ => {}
        }
        i += 1;
    }

    if as_json {
        headers.push(("Content-Type".to_string(), "application/json".to_string()));
    }
    let header_bytes = headers.iter().fold(0usize, |total, (name, value)| {
        total.saturating_add(name.len()).saturating_add(value.len())
    });
    if headers.len() > MAX_HTTP_HEADERS
        || header_bytes > MAX_HTTP_HEADER_BYTES
        || headers.iter().any(|(name, value)| {
            name.is_empty()
                || name.chars().any(|ch| ch.is_control() || ch.is_whitespace())
                || value.chars().any(char::is_control)
        })
    {
        let msg = "http: headers are malformed or exceed their count/byte limit".to_string();
        eprintln!("{}", msg);
        state.set_error(msg, 1);
        return Err(1);
    }

    // Body: explicit -d wins; else use the pipeline input if non-empty.
    let body_bytes: Option<Vec<u8>> = if let Some(b) = body_arg {
        if b.len() > MAX_HTTP_BODY_BYTES {
            let msg = format!("http: request body exceeds the {MAX_HTTP_BODY_BYTES} byte limit");
            eprintln!("{}", msg);
            state.set_error(msg, 1);
            return Err(1);
        }
        Some(b.into_bytes())
    } else {
        match input {
            PipelineData::Empty => None,
            PipelineData::Bytes(b) if b.len() > MAX_HTTP_BODY_BYTES => {
                let msg =
                    format!("http: request body exceeds the {MAX_HTTP_BODY_BYTES} byte limit");
                eprintln!("{}", msg);
                state.set_error(msg, 1);
                return Err(1);
            }
            PipelineData::Bytes(b) if !b.is_empty() => Some(b),
            PipelineData::Bytes(_) => None,
            PipelineData::Values(vs) => bounded_http_values(vs.into_iter()).map_err(|error| {
                let msg = format!("http: request body serialization failed: {error}");
                eprintln!("{msg}");
                state.set_error(msg, 1);
                1
            })?,
            PipelineData::Stream(it) => bounded_http_values(it).map_err(|error| {
                let msg = format!("http: request body serialization failed: {error}");
                eprintln!("{msg}");
                state.set_error(msg, 1);
                1
            })?,
        }
    };

    let request_url = append_query(&url, &query);
    if request_url.len() > MAX_HTTP_URL_BYTES {
        let msg =
            format!("http: URL exceeds the {MAX_HTTP_URL_BYTES} byte limit after query encoding");
        eprintln!("{}", msg);
        state.set_error(msg, 1);
        return Err(1);
    }

    let (status, header_rec, body_string) = {
        let agent = http_agent();

        let mut builder = ureq::http::Request::builder()
            .method(method)
            .uri(request_url);
        for (k, v) in &headers {
            builder = builder.header(k.as_str(), v.as_str());
        }

        let response = match body_bytes.as_deref() {
            Some(b) => builder
                .body(b)
                .map_err(|e| e.to_string())
                .and_then(|req| agent.run(req).map_err(|e| e.to_string())),
            None => builder
                .body(())
                .map_err(|e| e.to_string())
                .and_then(|req| agent.run(req).map_err(|e| e.to_string())),
        };

        let mut resp = match response {
            Ok(r) => r,
            Err(e) => {
                let msg = format!("http: request failed: {}", e);
                eprintln!("{}", msg);
                state.set_error(msg, 1);
                return Err(1);
            }
        };

        let status = resp.status().as_u16() as i64;
        let mut header_rec = indexmap::IndexMap::new();
        for (name, value) in resp.headers() {
            header_rec.insert(
                name.as_str().to_string(),
                Value::String(value.to_str().unwrap_or_default().to_string()),
            );
        }
        let body_string = match resp
            .body_mut()
            .with_config()
            .limit(MAX_HTTP_BODY_BYTES as u64)
            .read_to_string()
        {
            Ok(s) => s,
            Err(e) => {
                let msg = format!("http: body read failed: {}", e);
                eprintln!("{}", msg);
                state.set_error(msg, 1);
                return Err(1);
            }
        };
        (status, header_rec, body_string)
    };

    let content_type = header_rec
        .get("content-type")
        .or_else(|| header_rec.get("Content-Type"))
        .and_then(|v| {
            if let Value::String(s) = v {
                Some(s.clone())
            } else {
                None
            }
        })
        .unwrap_or_default();
    let body_value = if content_type.contains("json") {
        match serde_json::from_str::<serde_json::Value>(&body_string) {
            Ok(j) => Value::from_json(j),
            Err(_) => Value::String(body_string),
        }
    } else {
        Value::String(body_string)
    };

    let mut rec = indexmap::IndexMap::new();
    rec.insert("status".to_string(), Value::Int(status));
    rec.insert("headers".to_string(), Value::Record(header_rec));
    rec.insert("body".to_string(), body_value);
    Ok(PipelineData::Values(vec![Value::Record(rec)]))
}

#[cfg(feature = "ai")]
fn http_agent() -> ureq::Agent {
    ureq::Agent::config_builder()
        // User-supplied headers may be credentials unknown to ureq. Return a
        // 3xx record and let the script explicitly choose the next origin.
        .max_redirects(0)
        .timeout_connect(Some(std::time::Duration::from_secs(10)))
        .timeout_recv_response(Some(std::time::Duration::from_secs(30)))
        .timeout_recv_body(Some(std::time::Duration::from_secs(30)))
        .http_status_as_error(false)
        .build()
        .into()
}

#[cfg(not(feature = "ai"))]
fn vb_http(
    _input: PipelineData,
    _args: &[String],
    state: &mut ShellState,
) -> Result<PipelineData, i32> {
    let msg = "http: not available — jsh was built without the `ai` feature".to_string();
    eprintln!("{}", msg);
    state.set_error(msg, 1);
    Err(1)
}

#[cfg(all(test, feature = "ai"))]
mod http_boundary_tests {
    use super::*;

    #[test]
    fn typed_http_bodies_are_incrementally_bounded() {
        assert_eq!(http_agent().config().max_redirects(), 0);
        let values = std::iter::repeat_with(|| Value::String("12345678".to_string()));
        assert!(bounded_http_values_with_limit(values, 64).is_err());

        let body = bounded_http_values_with_limit(
            [Value::Int(1), Value::String("two".to_string())].into_iter(),
            64,
        )
        .expect("bounded serialization")
        .expect("nonempty body");
        assert_eq!(
            body,
            br#"[1,"two"]
"#
        );
    }
}
