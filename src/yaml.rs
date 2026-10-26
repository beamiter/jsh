//! YAML conversion shared by pipelines and completion.
//!
//! Keep the shell's JSON-compatible model, including ordered fields and YAML
//! text for non-string keys, without retaining an unmaintained parser backend.
use serde::de::{MapAccess, SeqAccess, Visitor};
use serde::ser::SerializeMap;
use serde::{Deserialize, Deserializer, Serialize, Serializer};
use serde_json::Value as Json;
use serde_saphyr::{Error, Span, Spanned, Tagged};

pub(crate) fn from_slice(bytes: &[u8]) -> Result<Json, Error> {
    parse(bytes, false)
}

/// Configuration completion only names string fields. In particular a numeric
/// Docker Compose service key must not turn into a suggested service name.
pub(crate) fn from_slice_string_keys_only(bytes: &[u8]) -> Result<Json, Error> {
    parse(bytes, true)
}

fn parse(bytes: &[u8], string_keys_only: bool) -> Result<Json, Error> {
    let source = std::str::from_utf8(bytes).map_err(|_| Error::InvalidUtf8Input)?;
    let options = serde_saphyr::options! {
        // Node/tag/span visitors need more stack than a direct typed parse.
        // Reject deep input before exhausting a normal 2 MiB worker stack.
        budget: serde_saphyr::budget! { max_depth: 32 },
        strict_booleans: true,
        merge_keys: serde_saphyr::MergeKeyPolicy::AsOrdinary,
        ignore_binary_tag_for_string: true,
        non_finite_float_policy: serde_saphyr::NonFiniteFloatPolicy::PassThrough,
        // Retain the parser's default scalar, node and alias budgets.
        // Optional include and property expansion features are not enabled.
    };
    let mut node: Node = serde_saphyr::from_str_with_options(source, options)?;
    node.normalize_scalars(source);
    node.into_json(string_keys_only)
}

// A JSON value cannot represent arbitrary YAML mapping keys or non-finite
// numbers. Keep those types until each key has been rendered as YAML text.
// Source spans let us retain the existing implicit scalar rules without
// rewriting the source document or implementing another YAML parser.
struct Node {
    kind: Kind,
    tag: Option<String>,
    span: Span,
}

enum Kind {
    Null,
    Bool(bool),
    Int(i64),
    UInt(u64),
    Float(f64),
    String(String),
    Sequence(Vec<Node>),
    Mapping(Vec<(Node, Node)>),
}

impl Node {
    fn normalize_scalars(&mut self, source: &str) {
        match &mut self.kind {
            Kind::Sequence(values) => {
                for value in values {
                    value.normalize_scalars(source);
                }
            }
            Kind::Mapping(entries) => {
                for (key, value) in entries {
                    key.normalize_scalars(source);
                    value.normalize_scalars(source);
                }
            }
            _ => {}
        }

        // Explicit core tags request a type. Local application tags do not
        // change implicit scalar inference; they are retained only for keys.
        if self.tag.as_deref().is_some_and(|tag| !tag.starts_with('!')) {
            return;
        }
        let Some(raw) = self.source_scalar(source) else {
            return;
        };
        let restore_string = match self.kind {
            Kind::Bool(_) => !matches!(raw, "true" | "True" | "TRUE" | "false" | "False" | "FALSE"),
            Kind::Null => !matches!(raw, "" | "~" | "null" | "Null" | "NULL"),
            Kind::Int(_) | Kind::UInt(_) | Kind::Float(_) => {
                let unsigned = raw.strip_prefix(['+', '-']).unwrap_or(raw);
                let leading_zero_integer = unsigned.len() > 1
                    && unsigned.starts_with('0')
                    && unsigned.bytes().all(|byte| byte.is_ascii_digit());
                let overflowing_float = matches!(self.kind, Kind::Float(value) if !value.is_finite())
                    && unsigned.as_bytes().first().is_some_and(u8::is_ascii_digit);
                raw.contains('_') || leading_zero_integer || overflowing_float
            }
            _ => false,
        };
        if restore_string {
            self.kind = Kind::String(raw.to_owned());
        }
    }

    fn source_scalar<'a>(&self, source: &'a str) -> Option<&'a str> {
        let start = usize::try_from(self.span.byte_offset()?).ok()?;
        let len = usize::try_from(self.span.byte_len()?).ok()?;
        source.get(start..start.checked_add(len)?)
    }

    fn into_json(self, string_keys_only: bool) -> Result<Json, Error> {
        Ok(match self.kind {
            Kind::Null => Json::Null,
            Kind::Bool(value) => Json::Bool(value),
            Kind::Int(value) => Json::Number(value.into()),
            Kind::UInt(value) => Json::Number(value.into()),
            Kind::Float(value) => serde_json::Number::from_f64(value)
                .map(Json::Number)
                .unwrap_or(Json::Null),
            Kind::String(value) => Json::String(value),
            Kind::Sequence(values) => Json::Array(
                values
                    .into_iter()
                    .map(|value| value.into_json(string_keys_only))
                    .collect::<Result<_, _>>()?,
            ),
            Kind::Mapping(entries) => {
                let mut object = serde_json::Map::new();
                for (key, value) in entries {
                    if string_keys_only && !matches!(key.kind, Kind::String(_)) {
                        continue;
                    }
                    let name = match key {
                        Node {
                            kind: Kind::String(name),
                            ref tag,
                            ..
                        } if string_keys_only || !is_local_tag(tag.as_deref()) => name,
                        other => serde_saphyr::to_string(&other)
                            .map_err(<Error as serde::de::Error>::custom)?
                            .trim()
                            .to_owned(),
                    };
                    match object.entry(name) {
                        serde_json::map::Entry::Occupied(entry) => {
                            return Err(<Error as serde::de::Error>::custom(format!(
                                "duplicate mapping key after YAML-to-JSON conversion: {:?}",
                                entry.key()
                            )));
                        }
                        serde_json::map::Entry::Vacant(entry) => {
                            entry.insert(value.into_json(string_keys_only)?);
                        }
                    }
                }
                Json::Object(object)
            }
        })
    }
}

fn is_local_tag(tag: Option<&str>) -> bool {
    tag.is_some_and(|tag| tag.starts_with('!') && tag.len() > 1)
}

impl<'de> Deserialize<'de> for Node {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        let value = Spanned::<Tagged<Kind>>::deserialize(deserializer)?;
        Ok(Self {
            kind: value.value.0,
            tag: value.value.1,
            span: value.defined.span(),
        })
    }
}

impl<'de> Deserialize<'de> for Kind {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        struct NodeVisitor;

        impl<'de> Visitor<'de> for NodeVisitor {
            type Value = Kind;

            fn expecting(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
                formatter.write_str("a YAML value")
            }

            fn visit_unit<E>(self) -> Result<Kind, E> {
                Ok(Kind::Null)
            }

            fn visit_bool<E>(self, value: bool) -> Result<Kind, E> {
                Ok(Kind::Bool(value))
            }

            fn visit_i64<E>(self, value: i64) -> Result<Kind, E> {
                Ok(Kind::Int(value))
            }

            fn visit_u64<E>(self, value: u64) -> Result<Kind, E> {
                Ok(Kind::UInt(value))
            }

            fn visit_f64<E>(self, value: f64) -> Result<Kind, E> {
                Ok(Kind::Float(value))
            }

            fn visit_str<E>(self, value: &str) -> Result<Kind, E> {
                Ok(Kind::String(value.to_owned()))
            }

            fn visit_string<E>(self, value: String) -> Result<Kind, E> {
                Ok(Kind::String(value))
            }

            fn visit_seq<A: SeqAccess<'de>>(self, mut sequence: A) -> Result<Kind, A::Error> {
                let mut values = Vec::new();
                while let Some(value) = sequence.next_element()? {
                    values.push(value);
                }
                Ok(Kind::Sequence(values))
            }

            fn visit_map<A: MapAccess<'de>>(self, mut map: A) -> Result<Kind, A::Error> {
                let mut entries = Vec::new();
                while let Some(entry) = map.next_entry()? {
                    entries.push(entry);
                }
                Ok(Kind::Mapping(entries))
            }
        }

        deserializer.deserialize_any(NodeVisitor)
    }
}

impl Serialize for Node {
    fn serialize<S: Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        if is_local_tag(self.tag.as_deref()) {
            Tagged(&self.kind, self.tag.clone()).serialize(serializer)
        } else {
            self.kind.serialize(serializer)
        }
    }
}

impl Serialize for Kind {
    fn serialize<S: Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        match self {
            Self::Null => serializer.serialize_unit(),
            Self::Bool(value) => serializer.serialize_bool(*value),
            Self::Int(value) => serializer.serialize_i64(*value),
            Self::UInt(value) => serializer.serialize_u64(*value),
            Self::Float(value) => serializer.serialize_f64(*value),
            Self::String(value) => serializer.serialize_str(value),
            Self::Sequence(values) => values.serialize(serializer),
            Self::Mapping(entries) => {
                let mut map = serializer.serialize_map(Some(entries.len()))?;
                for (key, value) in entries {
                    map.serialize_entry(key, value)?;
                }
                map.end()
            }
        }
    }
}
