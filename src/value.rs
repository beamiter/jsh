/// Typed pipeline values (nushell-style).
///
/// Phase 5a: foundation for in-process structured pipelines.
/// `Closure` is declared but only populated in Phase 5b.
use indexmap::IndexMap;
use serde_json::{Number, Value as JsonValue};
use std::collections::HashMap;
use std::fmt;
use std::sync::Arc;
use unicode_width::UnicodeWidthStr;

#[derive(Debug, Clone)]
pub enum Value {
    Null,
    Bool(bool),
    Int(i64),
    Float(f64),
    String(String),
    List(Vec<Value>),
    Record(IndexMap<String, Value>),
    Binary(Vec<u8>),
    Closure(Arc<ClosureData>),
}

/// Closure payload. `captured` is a by-value snapshot of let_vars taken at
/// definition time so the closure is a pure value (no aliasing surprises).
#[derive(Debug, Clone)]
pub struct ClosureData {
    pub params: Vec<String>,
    pub body_src: String,
    pub captured: HashMap<String, Value>,
}

impl ClosureData {
    /// Resolve a closure-local binding without cloning its captured scope or
    /// argument records. Missing parameters still shadow captures with Null.
    pub(crate) fn lookup<'a>(&'a self, args: &'a [Value], name: &str) -> Option<&'a Value> {
        if let Some(index) = self.params.iter().rposition(|parameter| parameter == name) {
            return Some(args.get(index).unwrap_or(&Value::Null));
        }
        self.captured
            .get(name)
            .or_else(|| (name == "in").then(|| args.first().unwrap_or(&Value::Null)))
    }
}

impl PartialEq for Value {
    fn eq(&self, other: &Self) -> bool {
        match (self, other) {
            (Value::Null, Value::Null) => true,
            (Value::Bool(a), Value::Bool(b)) => a == b,
            (Value::Int(a), Value::Int(b)) => a == b,
            (Value::Float(a), Value::Float(b)) => a == b,
            (Value::Int(a), Value::Float(b)) | (Value::Float(b), Value::Int(a)) => {
                compare_int_float(*a, *b) == Some(std::cmp::Ordering::Equal)
            }
            (Value::String(a), Value::String(b)) => a == b,
            (Value::List(a), Value::List(b)) => a == b,
            (Value::Record(a), Value::Record(b)) => {
                a.len() == b.len()
                    && a.iter()
                        .zip(b.iter())
                        .all(|((ka, va), (kb, vb))| ka == kb && va == vb)
            }
            (Value::Binary(a), Value::Binary(b)) => a == b,
            _ => false,
        }
    }
}

/// Compare without rounding an integer to f64 first. Above 2^53, adjacent
/// integers no longer have distinct floating-point representations.
pub(crate) fn compare_int_float(integer: i64, float: f64) -> Option<std::cmp::Ordering> {
    use std::cmp::Ordering;
    if float.is_nan() {
        return None;
    }
    if float >= 9_223_372_036_854_775_808.0 {
        return Some(Ordering::Less);
    }
    if float < -9_223_372_036_854_775_808.0 {
        return Some(Ordering::Greater);
    }
    match integer.cmp(&(float as i64)) {
        Ordering::Equal if float.fract() > 0.0 => Some(Ordering::Less),
        Ordering::Equal if float.fract() < 0.0 => Some(Ordering::Greater),
        order => Some(order),
    }
}

/// Numeric projection shared by comparisons and exact aggregations. Integer
/// strings and booleans retain their integral representation; explicitly
/// floating-point input stays floating point.
#[derive(Debug, Clone, Copy)]
pub(crate) enum NumericValue {
    Int(i64),
    Float(f64),
}

impl NumericValue {
    pub(crate) fn parse(text: &str) -> Option<Self> {
        text.parse::<i64>()
            .map(Self::Int)
            .ok()
            .or_else(|| text.parse::<f64>().ok().map(Self::Float))
    }

    pub(crate) fn as_f64(self) -> f64 {
        match self {
            Self::Int(number) => number as f64,
            Self::Float(number) => number,
        }
    }

    pub(crate) fn is_nan(self) -> bool {
        matches!(self, Self::Float(number) if number.is_nan())
    }

    pub(crate) fn compare(self, other: Self) -> Option<std::cmp::Ordering> {
        match (self, other) {
            (Self::Int(a), Self::Int(b)) => Some(a.cmp(&b)),
            (Self::Int(a), Self::Float(b)) => compare_int_float(a, b),
            (Self::Float(a), Self::Int(b)) => {
                compare_int_float(b, a).map(std::cmp::Ordering::reverse)
            }
            (Self::Float(a), Self::Float(b)) => a.partial_cmp(&b),
        }
    }

    fn sort_order(self, other: Self) -> std::cmp::Ordering {
        // Relational comparisons remain unordered for NaN. Sorting needs a
        // total order, so all NaNs form a stable group after other numbers.
        self.compare(other)
            .unwrap_or_else(|| self.is_nan().cmp(&other.is_nan()))
    }
}

impl Value {
    /// Best-effort string projection for comparison/display.
    pub fn to_display_string(&self) -> String {
        match self {
            Value::Null => String::new(),
            Value::Bool(b) => b.to_string(),
            Value::Int(i) => i.to_string(),
            Value::Float(f) => {
                if f.fract() == 0.0 && f.abs() < 1e16 {
                    format!("{}", *f as i64)
                } else {
                    f.to_string()
                }
            }
            Value::String(s) => s.clone(),
            Value::List(items) => {
                let inner: Vec<String> = items.iter().map(|v| v.to_display_string()).collect();
                format!("[{}]", inner.join(", "))
            }
            Value::Record(map) => {
                let inner: Vec<String> = map
                    .iter()
                    .map(|(k, v)| format!("{}: {}", k, v.to_display_string()))
                    .collect();
                format!("{{{}}}", inner.join(", "))
            }
            Value::Binary(b) => format!("<{} bytes>", b.len()),
            Value::Closure(_) => "<closure>".to_string(),
        }
    }

    /// Try to coerce to f64 for numeric comparisons / math.
    pub fn as_f64(&self) -> Option<f64> {
        match self {
            Value::Int(i) => Some(*i as f64),
            Value::Float(f) => Some(*f),
            Value::String(s) => s.parse::<f64>().ok(),
            Value::Bool(b) => Some(if *b { 1.0 } else { 0.0 }),
            _ => None,
        }
    }

    pub(crate) fn as_number(&self) -> Option<NumericValue> {
        match self {
            Self::Int(number) => Some(NumericValue::Int(*number)),
            Self::Float(number) => Some(NumericValue::Float(*number)),
            Self::String(text) => NumericValue::parse(text),
            Self::Bool(value) => Some(NumericValue::Int(i64::from(*value))),
            _ => None,
        }
    }

    /// Shared ordering for `sort` and `sort-by`: null, numeric values, then
    /// other values by their display text. Fixed classes keep the comparison
    /// transitive when numeric and nonnumeric strings are mixed.
    pub(crate) fn compare_for_sort(&self, other: &Self) -> std::cmp::Ordering {
        use std::cmp::Ordering;
        match (self, other) {
            (Self::Null, Self::Null) => Ordering::Equal,
            (Self::Null, _) => Ordering::Less,
            (_, Self::Null) => Ordering::Greater,
            _ => match (self.as_number(), other.as_number()) {
                (Some(a), Some(b)) => a.sort_order(b),
                (Some(_), None) => Ordering::Less,
                (None, Some(_)) => Ordering::Greater,
                (None, None) => self.to_display_string().cmp(&other.to_display_string()),
            },
        }
    }

    pub fn is_record(&self) -> bool {
        matches!(self, Value::Record(_))
    }

    pub fn get(&self, key: &str) -> Option<&Value> {
        match self {
            Value::Record(map) => map.get(key),
            _ => None,
        }
    }

    /// Convert from `serde_json::Value` (boundary adapter for the fork path).
    pub fn from_json(j: JsonValue) -> Value {
        match j {
            JsonValue::Null => Value::Null,
            JsonValue::Bool(b) => Value::Bool(b),
            JsonValue::Number(n) => {
                if let Some(i) = n.as_i64() {
                    Value::Int(i)
                } else if let Some(f) = n.as_f64() {
                    Value::Float(f)
                } else {
                    Value::String(n.to_string())
                }
            }
            JsonValue::String(s) => Value::String(s),
            JsonValue::Array(arr) => Value::List(arr.into_iter().map(Value::from_json).collect()),
            JsonValue::Object(obj) => {
                let mut map = IndexMap::with_capacity(obj.len());
                for (k, v) in obj {
                    map.insert(k, Value::from_json(v));
                }
                Value::Record(map)
            }
        }
    }

    /// Lossy round-trip to JSON. Binary→base64-ish, Closure→Null.
    pub fn to_json(&self) -> JsonValue {
        match self {
            Value::Null => JsonValue::Null,
            Value::Bool(b) => JsonValue::Bool(*b),
            Value::Int(i) => JsonValue::Number((*i).into()),
            Value::Float(f) => Number::from_f64(*f)
                .map(JsonValue::Number)
                .unwrap_or(JsonValue::Null),
            Value::String(s) => JsonValue::String(s.clone()),
            Value::List(items) => JsonValue::Array(items.iter().map(|v| v.to_json()).collect()),
            Value::Record(map) => {
                let mut obj = serde_json::Map::with_capacity(map.len());
                for (k, v) in map {
                    obj.insert(k.clone(), v.to_json());
                }
                JsonValue::Object(obj)
            }
            Value::Binary(b) => JsonValue::String(format!("<binary {} bytes>", b.len())),
            Value::Closure(_) => JsonValue::Null,
        }
    }
}

impl fmt::Display for Value {
    /// "Show" form for top-level pipeline output.
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Value::List(items) if items.iter().all(|v| v.is_record()) && !items.is_empty() => {
                f.write_str(&render_table(items))
            }
            Value::Record(map) => {
                for (k, v) in map {
                    writeln!(f, "{}: {}", k, v.to_display_string())?;
                }
                Ok(())
            }
            Value::List(items) => {
                for item in items {
                    writeln!(f, "{}", item.to_display_string())?;
                }
                Ok(())
            }
            _ => f.write_str(&self.to_display_string()),
        }
    }
}

/// Render a list of Records as an aligned table.
/// Moved from `structured.rs::to_table` and rewritten for `Value`.
pub fn render_table(records: &[Value]) -> String {
    if records.is_empty() {
        return String::new();
    }

    let mut columns: Vec<String> = Vec::new();
    for rec in records {
        if let Value::Record(map) = rec {
            for key in map.keys() {
                if !columns.iter().any(|c| c == key) {
                    columns.push(key.clone());
                }
            }
        }
    }
    if columns.is_empty() {
        return String::new();
    }

    let mut rows: Vec<Vec<String>> = Vec::new();
    rows.push(columns.clone());
    for rec in records {
        let row: Vec<String> = columns
            .iter()
            .map(|col| {
                rec.get(col)
                    .map(|v| v.to_display_string())
                    .unwrap_or_default()
            })
            .collect();
        rows.push(row);
    }

    let mut widths: Vec<usize> = columns.iter().map(|c| c.width()).collect();
    for row in &rows[1..] {
        for (i, cell) in row.iter().enumerate() {
            if i < widths.len() {
                widths[i] = widths[i].max(cell.width());
            }
        }
    }

    let mut out = String::new();
    for (ri, row) in rows.iter().enumerate() {
        for (i, cell) in row.iter().enumerate() {
            if i > 0 {
                out.push_str("  ");
            }
            let w = widths.get(i).copied().unwrap_or(0);
            out.push_str(cell);
            out.push_str(&" ".repeat(w.saturating_sub(cell.width())));
        }
        out.push('\n');
        if ri == 0 {
            for (i, w) in widths.iter().enumerate() {
                if i > 0 {
                    out.push_str("  ");
                }
                out.push_str(&"-".repeat(*w));
            }
            out.push('\n');
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn json_round_trip_preserves_order() {
        let src = r#"{"b":1,"a":2,"c":3}"#;
        let j: JsonValue = serde_json::from_str(src).unwrap();
        let v = Value::from_json(j);
        if let Value::Record(map) = &v {
            let keys: Vec<&String> = map.keys().collect();
            assert_eq!(keys, vec!["b", "a", "c"]);
        } else {
            panic!("expected record");
        }
        let back = v.to_json();
        assert_eq!(serde_json::to_string(&back).unwrap(), src);
    }

    #[test]
    fn display_record_list_renders_table() {
        let r1 = {
            let mut m = IndexMap::new();
            m.insert("a".to_string(), Value::Int(1));
            m.insert("b".to_string(), Value::String("xx".to_string()));
            Value::Record(m)
        };
        let r2 = {
            let mut m = IndexMap::new();
            m.insert("a".to_string(), Value::Int(22));
            m.insert("b".to_string(), Value::String("y".to_string()));
            Value::Record(m)
        };
        let s = format!("{}", Value::List(vec![r1, r2]));
        assert!(s.contains("a "));
        assert!(s.contains("22"));
    }

    #[test]
    fn as_f64_coercions() {
        assert_eq!(Value::Int(5).as_f64(), Some(5.0));
        assert_eq!(Value::Float(2.5).as_f64(), Some(2.5));
        assert_eq!(Value::String("2.5".to_string()).as_f64(), Some(2.5));
        assert_eq!(Value::Bool(true).as_f64(), Some(1.0));
        assert_eq!(Value::Null.as_f64(), None);
    }

    #[test]
    fn mixed_sort_order_is_antisymmetric_and_transitive() {
        let values = [
            Value::Null,
            Value::Int(i64::MIN),
            Value::Int(i64::MIN + 1),
            Value::Int(9_007_199_254_740_993),
            Value::Int(i64::MAX),
            Value::Int(2),
            Value::Int(10),
            Value::Float(-1.5),
            Value::Float(-0.0),
            Value::Float(9_007_199_254_740_992.0),
            Value::Float(f64::NEG_INFINITY),
            Value::Float(f64::INFINITY),
            Value::Float(f64::NAN),
            Value::Bool(false),
            Value::Bool(true),
            Value::String("9007199254740993".to_owned()),
            Value::String("1x".to_owned()),
            Value::String("!".to_owned()),
            Value::String("NaN".to_owned()),
            Value::List(vec![Value::Int(1)]),
            Value::Record(IndexMap::new()),
        ];
        for a in &values {
            for b in &values {
                assert_eq!(a.compare_for_sort(b), b.compare_for_sort(a).reverse());
                for c in &values {
                    if a.compare_for_sort(b).is_le() && b.compare_for_sort(c).is_le() {
                        assert!(a.compare_for_sort(c).is_le(), "{a:?}, {b:?}, {c:?}");
                    }
                }
            }
        }
    }
}
