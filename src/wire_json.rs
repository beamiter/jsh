//! Byte-canonical JSON for everything jsh puts on a provider's wire.
//!
//! jsh builds `serde_json` with `preserve_order` on purpose: its structured
//! pipeline promises that a record keeps the field order it was read or built
//! with, and `Value::to_json`/`from_json`, `from-yaml`, `from-toml`,
//! `from-xml` and the whole of `structured.rs` all carry that order through a
//! `serde_json::Map`. Turning the feature off would silently re-alphabetise
//! every table column and every `to json` document, so it stays on.
//!
//! Cargo features, however, are unified across the whole dependency graph, not
//! per dependent. `jagent` assembles each provider request body as a
//! `serde_json::Map` (`json!({...})` plus `body["temperature"] = ...`), so it
//! inherits jsh's feature and emits members in *insertion* order here while
//! emitting them in *sorted* order inside anvil/forge/ember/frost, which build
//! `serde_json` plain. Same jagent code, two different request bodies on the
//! wire — and jsh's variant is the one no jagent CI lane ever builds.
//!
//! jsh cannot un-unify a feature, so it pins the bytes instead: every outbound
//! provider body passes through [`canonical_request_body`], which re-emits the
//! object with members in lexicographic order at every level — exactly what a
//! `BTreeMap`-backed `serde_json::Map` produces. jsh then sends the same bytes
//! as the rest of the family, and keeps sending them no matter which crate in
//! the graph flips `preserve_order` next.
//!
//! JSON object member order carries no meaning, so this never changes what a
//! provider is asked to do; it only removes a build-configuration variable
//! from the request bytes.

use serde_json::Value;

/// Re-encode one already-serialized JSON request body with its members in
/// canonical (byte-lexicographic) order.
///
/// Reordering members cannot change the encoded length, so a length change
/// means this pass *rewrote* the request rather than reordering it — a
/// duplicate member collapsing into one, or a number re-formatting. The
/// request is refused in that case rather than sent in a shape its builder
/// never produced. Nesting depth is already bounded by serde_json's own
/// recursion limit, which rejects the input before the recursion below runs.
pub fn canonical_request_body(body: &str) -> Result<String, String> {
    let value: Value = serde_json::from_str(body)
        .map_err(|error| format!("outbound request body is not valid JSON: {error}"))?;
    if !value.is_object() {
        return Err("outbound request body is not a JSON object".to_string());
    }
    let canonical = serde_json::to_string(&with_sorted_members(value))
        .map_err(|error| format!("outbound request body could not be re-encoded: {error}"))?;
    if canonical.len() != body.len() {
        return Err(
            "outbound request body changed size while being put in canonical order".to_string(),
        );
    }
    Ok(canonical)
}

/// Rebuild every object in `value` by inserting its members in sorted order.
///
/// Insertion order is what matters: under `preserve_order` a `serde_json::Map`
/// iterates in insertion order, so inserting sorted makes it serialize exactly
/// like the `BTreeMap` a build without the feature would use. Without the
/// feature the map sorts anyway and this is a no-op.
fn with_sorted_members(value: Value) -> Value {
    match value {
        Value::Array(items) => Value::Array(items.into_iter().map(with_sorted_members).collect()),
        Value::Object(members) => {
            let mut members: Vec<(String, Value)> = members.into_iter().collect();
            members.sort_by(|(left, _), (right, _)| left.cmp(right));
            Value::Object(
                members
                    .into_iter()
                    .map(|(key, value)| (key, with_sorted_members(value)))
                    .collect(),
            )
        }
        scalar => scalar,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The pin that makes the feature invisible: this exact string is what a
    /// `serde_json` built *without* `preserve_order` emits for the same value,
    /// so it must come out of jsh's `preserve_order` build too.
    #[test]
    fn members_are_sorted_at_every_level_of_the_body() {
        let body = r#"{"model":"m","max_tokens":200,"messages":[{"role":"user","content":"hi"}],"temperature":0.1,"stream":true,"options":{"num_predict":200,"temperature":0.1}}"#;
        assert_eq!(
            canonical_request_body(body).unwrap(),
            r#"{"max_tokens":200,"messages":[{"content":"hi","role":"user"}],"model":"m","options":{"num_predict":200,"temperature":0.1},"stream":true,"temperature":0.1}"#
        );
    }

    #[test]
    fn canonical_bodies_are_unchanged_and_the_pass_is_idempotent() {
        let canonical =
            r#"{"max_tokens":200,"messages":[{"content":"hi","role":"user"}],"model":"m"}"#;
        let once = canonical_request_body(canonical).unwrap();
        assert_eq!(once, canonical);
        assert_eq!(canonical_request_body(&once).unwrap(), canonical);
    }

    /// Sorting is byte-lexicographic like `BTreeMap<String, _>`, not
    /// case-insensitive and not locale-aware.
    #[test]
    fn member_order_is_byte_lexicographic() {
        let body = r#"{"b":1,"A":2,"a":3,"B":4,"é":5,"z":6}"#;
        assert_eq!(
            canonical_request_body(body).unwrap(),
            r#"{"A":2,"B":4,"a":3,"b":1,"z":6,"é":5}"#
        );
    }

    #[test]
    fn a_body_that_is_not_one_json_object_is_refused() {
        for body in ["[]", "\"text\"", "null", "{", ""] {
            assert!(canonical_request_body(body).is_err(), "{body}");
        }
    }

    /// A duplicate member would be silently collapsed by the round trip, which
    /// is a rewrite rather than a reordering. The size guard catches it.
    #[test]
    fn a_body_that_would_be_rewritten_rather_than_reordered_is_refused() {
        let error = canonical_request_body(r#"{"model":"a","model":"b"}"#).unwrap_err();
        assert!(error.contains("changed size"), "{error}");
    }
}
