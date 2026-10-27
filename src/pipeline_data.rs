/// Pipeline data carrier — bytes vs typed values.
///
/// Phase 15b adds a `Stream` variant: a lazy iterator of `Value`s.
/// Consumers that don't need full materialization (`take`, `first`,
/// `where` short-circuit, `to-json` streaming, etc.) can drain it
/// incrementally; legacy code paths transparently `collect()` via
/// `into_values()`.
use crate::value::Value;
use std::cell::RefCell;
use std::io::{self, Write};

pub type ValueStream = Box<dyn Iterator<Item = Value> + Send>;

thread_local! {
    /// Why a lazy stage ended its stream early, until a consumer reports it.
    ///
    /// Stream items are plain values with no error variant, so a stage that fails
    /// while producing rows (a closure error at row N, a malformed line under
    /// `from-ndjson --strict`) ends its iterator and leaves the reason here. Code
    /// that drains a stream checks this before trusting what it collected, so a
    /// truncated input never passes for a complete one. Pipeline runners clear it
    /// on entry, so a failure from an abandoned pipeline cannot leak into the next.
    ///
    /// It is per thread: a pipeline's stages are built and drained on the thread
    /// running that pipeline (`par-each` workers receive owned chunks, never a
    /// stream), and unrelated pipelines on other threads, such as concurrent
    /// tests, cannot observe each other's failures.
    static STREAM_FAILURE: RefCell<Option<String>> = const { RefCell::new(None) };
}

/// Record why a lazy stage stopped. The first failure wins: every later stage
/// only sees the truncated stream that failure left behind.
pub(crate) fn record_stream_failure(message: String) {
    STREAM_FAILURE.with(|slot| {
        slot.borrow_mut().get_or_insert(message);
    });
}

/// Claim the pending failure, if any, so exactly one place reports it.
pub(crate) fn take_stream_failure() -> Option<String> {
    STREAM_FAILURE.with(|slot| slot.borrow_mut().take())
}

fn stream_failure_pending() -> bool {
    STREAM_FAILURE.with(|slot| slot.borrow().is_some())
}

/// Forget a failure that an earlier, abandoned pipeline left unreported.
pub(crate) fn clear_stream_failure() {
    take_stream_failure();
}

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
            PipelineData::Stream(it) => {
                let values: Vec<Value> = it.collect();
                // A stage that failed ended the stream early. Computing over the
                // remainder would present partial input as complete, so fail and
                // leave the message pending for whoever reports this pipeline.
                if stream_failure_pending() {
                    return Err(1);
                }
                Ok(values)
            }
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

    /// Transform each value without pulling from a lazy stream until requested.
    /// Byte input is decoded eagerly, preserving the normal JSON boundary errors.
    pub fn map_values<F>(self, map: F) -> Result<Self, i32>
    where
        F: FnMut(Value) -> Value + Send + 'static,
    {
        match self {
            Self::Stream(values) => Ok(Self::Stream(Box::new(values.map(map)))),
            other => Ok(Self::Values(
                other.into_values()?.into_iter().map(map).collect(),
            )),
        }
    }

    /// Filter values while retaining laziness for streaming inputs.
    pub fn filter_values<F>(self, predicate: F) -> Result<Self, i32>
    where
        F: FnMut(&Value) -> bool + Send + 'static,
    {
        match self {
            Self::Stream(values) => Ok(Self::Stream(Box::new(values.filter(predicate)))),
            other => Ok(Self::Values(
                other.into_values()?.into_iter().filter(predicate).collect(),
            )),
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
        use std::os::fd::FromRawFd;
        // A result computed from a truncated stream (a count, a saved file, a
        // rendered table) must not be presented as the pipeline's answer.
        if let Some(message) = take_stream_failure() {
            return Err(io::Error::other(message));
        }
        if matches!(&self, PipelineData::Empty)
            || matches!(&self, PipelineData::Values(values) if values.is_empty())
        {
            return Ok(());
        }
        io::stdout().flush()?;
        // Bind the buffer to this exact destination. The process-wide stdout
        // buffer retains bytes after a failed flush (e.g. /dev/full); restoring
        // fd 1 would otherwise leak those bytes into the next command's output.
        let descriptor = unsafe { nix::libc::fcntl(1, nix::libc::F_DUPFD_CLOEXEC, 3) };
        if descriptor < 0 {
            return Err(io::Error::last_os_error());
        }
        let file = unsafe { std::fs::File::from_raw_fd(descriptor) };
        let mut output = io::LineWriter::new(file);
        match self {
            PipelineData::Empty => {}
            PipelineData::Bytes(bytes) => output.write_all(&bytes)?,
            PipelineData::Values(values) => {
                if values.len() == 1 {
                    // A scalar's display form has no line ending of its own;
                    // without one, `range 1..5 | math sum; echo next` printed
                    // `15next`. Records and tables already end their lines.
                    let mut tracked = LastByte::new(&mut output);
                    write!(tracked, "{}", values[0])?;
                    if tracked.last.is_some_and(|byte| byte != b'\n') {
                        output.write_all(b"\n")?;
                    }
                } else if values.iter().all(|value| value.is_record()) {
                    write!(output, "{}", crate::value::render_table(&values))?;
                } else {
                    for value in values {
                        writeln!(output, "{}", value.to_display_string())?;
                    }
                }
            }
            PipelineData::Stream(values) => {
                for value in values {
                    writeln!(output, "{}", value.to_display_string())?;
                }
                // Rows before a failing one are already out, as with any
                // stream; the failure still decides the pipeline's status.
                if let Some(message) = take_stream_failure() {
                    output.flush()?;
                    return Err(io::Error::other(message));
                }
            }
        }
        output.flush()
    }
}

/// Forwards writes while remembering the final byte, so a rendered value can
/// be terminated with a newline without first buffering its whole text.
struct LastByte<W> {
    inner: W,
    last: Option<u8>,
}

impl<W> LastByte<W> {
    fn new(inner: W) -> Self {
        Self { inner, last: None }
    }
}

impl<W: Write> Write for LastByte<W> {
    fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
        let written = self.inner.write(buf)?;
        if written > 0 {
            self.last = Some(buf[written - 1]);
        }
        Ok(written)
    }

    fn flush(&mut self) -> io::Result<()> {
        self.inner.flush()
    }
}

fn values_to_pretty_json_bytes(vs: &[Value]) -> Vec<u8> {
    let arr = serde_json::Value::Array(vs.iter().map(|v| v.to_json()).collect());
    let mut out = serde_json::to_vec_pretty(&arr).unwrap_or_default();
    out.push(b'\n');
    out
}
