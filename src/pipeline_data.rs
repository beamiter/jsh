/// Pipeline data carrier — bytes vs typed values.
///
/// Phase 15b adds a `Stream` variant: a lazy iterator of `Value`s.
/// Consumers that don't need full materialization (`take`, `first`,
/// `where` short-circuit, `to-json` streaming, etc.) can drain it
/// incrementally; legacy code paths transparently `collect()` via
/// `into_values()`.
use crate::value::Value;
use std::io::{self, Write};

pub type ValueStream = Box<dyn Iterator<Item = Value> + Send>;

pub enum PipelineData {
    Empty,
    Bytes(Vec<u8>),
    Values(Vec<Value>),
    /// Lazy stream of values. Producers can yield rows without
    /// materializing the whole list. `into_values()` collects.
    Stream(ValueStream),
}

impl std::fmt::Debug for PipelineData {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            PipelineData::Empty => write!(f, "Empty"),
            PipelineData::Bytes(b) => write!(f, "Bytes({} bytes)", b.len()),
            PipelineData::Values(v) => write!(f, "Values({} items)", v.len()),
            PipelineData::Stream(_) => write!(f, "Stream(..)"),
        }
    }
}

impl PipelineData {
    /// Coerce to `Vec<Value>`. Bytes are parsed as JSON (array or NDJSON).
    /// Streams are drained.
    pub fn into_values(self) -> Result<Vec<Value>, i32> {
        match self {
            PipelineData::Empty => Ok(Vec::new()),
            PipelineData::Values(v) => Ok(v),
            PipelineData::Stream(it) => Ok(it.collect()),
            PipelineData::Bytes(b) => {
                let s = String::from_utf8_lossy(&b);
                let s = s.trim();
                if s.is_empty() {
                    return Ok(Vec::new());
                }
                if let Ok(serde_json::Value::Array(arr)) = serde_json::from_str(s) {
                    return Ok(arr.into_iter().map(Value::from_json).collect());
                }
                // Try a single JSON value
                if let Ok(v) = serde_json::from_str::<serde_json::Value>(s) {
                    return Ok(vec![Value::from_json(v)]);
                }
                // NDJSON
                let mut out = Vec::new();
                for line in s.lines() {
                    let line = line.trim();
                    if line.is_empty() {
                        continue;
                    }
                    match serde_json::from_str::<serde_json::Value>(line) {
                        Ok(v) => out.push(Value::from_json(v)),
                        Err(_) => {
                            eprintln!("jsh: cannot parse pipeline input as JSON");
                            return Err(1);
                        }
                    }
                }
                Ok(out)
            }
        }
    }

    /// Return a uniform iterator over values regardless of variant. Bytes are
    /// eagerly decoded; Stream is consumed lazily.
    pub fn into_value_iter(self) -> Result<ValueStream, i32> {
        match self {
            PipelineData::Stream(it) => Ok(it),
            other => {
                let vs = other.into_values()?;
                Ok(Box::new(vs.into_iter()))
            }
        }
    }

    /// Whether the variant is a lazy Stream (useful for short-circuit ops).
    pub fn is_stream(&self) -> bool {
        matches!(self, PipelineData::Stream(_))
    }

    /// Serialize to bytes for the fork/pipe boundary.
    /// - `Values` / `Stream` → pretty JSON array (Stream is drained first)
    /// - `Bytes` → passthrough
    /// - `Empty` → empty
    pub fn into_bytes(self) -> Vec<u8> {
        match self {
            PipelineData::Empty => Vec::new(),
            PipelineData::Bytes(b) => b,
            PipelineData::Values(vs) => values_to_pretty_json_bytes(&vs),
            PipelineData::Stream(it) => {
                let vs: Vec<Value> = it.collect();
                values_to_pretty_json_bytes(&vs)
            }
        }
    }

    /// Print to stdout. Values use `Display` (table / record / scalar).
    /// Streams print each value on its own line as they arrive so a long
    /// pipeline doesn't have to buffer everything before any output.
    pub fn write_to_stdout(self) -> io::Result<()> {
        match self {
            PipelineData::Empty => Ok(()),
            PipelineData::Bytes(b) => {
                let stdout = io::stdout();
                let mut h = stdout.lock();
                h.write_all(&b)
            }
            PipelineData::Values(vs) => {
                if vs.is_empty() {
                    return Ok(());
                }
                let stdout = io::stdout();
                let mut h = stdout.lock();
                if vs.len() == 1 {
                    write!(h, "{}", vs[0])
                } else if vs.iter().all(|v| v.is_record()) {
                    write!(h, "{}", crate::value::render_table(&vs))
                } else {
                    for v in &vs {
                        writeln!(h, "{}", v.to_display_string())?;
                    }
                    Ok(())
                }
            }
            PipelineData::Stream(it) => {
                let stdout = io::stdout();
                let mut h = stdout.lock();
                for v in it {
                    writeln!(h, "{}", v.to_display_string())?;
                }
                Ok(())
            }
        }
    }
}

fn values_to_pretty_json_bytes(vs: &[Value]) -> Vec<u8> {
    let arr = serde_json::Value::Array(vs.iter().map(|v| v.to_json()).collect());
    let mut out = serde_json::to_vec_pretty(&arr).unwrap_or_default();
    out.push(b'\n');
    out
}
