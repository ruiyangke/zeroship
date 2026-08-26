//! Native superjson encoder/decoder, wire-compatible with npm `superjson@2.x`.
//!
//! ## Wire format
//!
//! ```text
//! { "json": <payload>, "meta": { "values": <values>, "v": 1 } }
//! ```
//!
//! `meta` is omitted when there are no rich-type tags — the common case for
//! pure-JSON payloads. `meta.values` takes one of three shapes:
//!
//! * **Object** `{path: tag, ...}` — when the root payload is plain JSON but
//!   contains rich-typed children at the listed paths.
//! * **Tag** (a single tag tuple, e.g. `["Date"]`) — when the root payload is
//!   itself a rich type with no nested rich children.
//! * **Tuple** `[tag, {path: tag, ...}]` — when the root is a rich container
//!   (set/map) and itself has rich-typed children at nested paths.
//!
//! ## Why we deviate from the spec table
//!
//! The original spec described tags as bare strings (`"Date"`) and Uint8Array
//! as base64. The actual npm 2.x output uses single-element tag tuples
//! (`["Date"]`) and emits typed arrays as `["typed-array", "<Ctor>"]` with
//! the byte payload as a JSON array. The npm fixtures in
//! `tests/superjson_fixtures/` are the ground truth.
//!
//! ## Path syntax
//!
//! Paths are dot-joined — array index 1 inside `items` is `"items.1"`. Map
//! children use the array-form path: a Date *key* of the first entry is
//! `"0.0"`; the corresponding value is `"0.1"`.
//!
//! ## Scope of this crate
//!
//! This crate handles the wire shape only — it does not reconstruct V8 rich
//! types. The runtime consumes the meta map and revives `Date`, `BigInt`,
//! `Map`, etc. inside the V8 isolate.

use indexmap::IndexMap;
use serde_json::{json, Map as JsonMap, Value};

/// Top-level wire envelope. `meta` is `None` for pure-JSON payloads.
#[derive(Debug, Clone, PartialEq)]
pub struct Envelope {
    pub json: Value,
    pub meta: Option<Meta>,
}

/// Tag table for an envelope.
///
/// `root` carries a tag when the entire payload is a rich type (e.g. a
/// `Date` serialized at the top). `values` carries dot-path → tag entries
/// for any rich-typed descendants. We use `IndexMap` (not `BTreeMap`) so
/// iteration order matches insertion order — the npm wire format relies on
/// stable insertion order, and we need byte-equality on round-trip.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct Meta {
    pub root: Option<MetaTag>,
    pub values: IndexMap<String, MetaTag>,
}

impl Meta {
    /// True when there is nothing to write to the wire — both `root` and
    /// `values` are empty. Used by `to_bytes` to elide the `meta` key.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.root.is_none() && self.values.is_empty()
    }
}

/// A rich-type tag. `WithChildren` is the recursive case where a container
/// (set/map) has rich-typed children at known sub-paths.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum MetaTag {
    Date,
    BigInt,
    Map,
    Set,
    Regexp,
    Url,
    Undefined,
    /// `Infinity`, `-Infinity`, or `NaN` — npm tags these as `["number"]`.
    Number,
    /// Typed array constructor name, e.g. `Uint8Array`, `Int32Array`.
    /// Wire form is `["typed-array", "<Ctor>"]`.
    TypedArray(String),
    /// Container tag plus nested child paths, e.g. a Set whose 0-th value is
    /// itself rich. Wire form: `[<inner-tag>, {path: tag, ...}]`. The child
    /// map preserves insertion order — see `Meta::values`.
    WithChildren(Box<MetaTag>, IndexMap<String, MetaTag>),
}

#[derive(Debug, thiserror::Error)]
pub enum Error {
    #[error("invalid JSON: {0}")]
    InvalidJson(String),
    #[error("malformed meta: {0}")]
    MalformedMeta(String),
    #[error("unknown tag: {0}")]
    UnknownTag(String),
}

// ---------------------------------------------------------------------------
// Public API
// ---------------------------------------------------------------------------

/// Wrap a plain `serde_json::Value` as an envelope.
///
/// This crate does not introspect the value for rich types — input is assumed
/// to already be a JSON shadow. The runtime produces those shadows from V8
/// values.
#[must_use]
pub fn encode(value: Value) -> Envelope {
    Envelope { json: value, meta: None }
}

/// Strip meta and return the JSON shadow.
///
/// Rich types are NOT reconstructed (no `Date`, no `BigInt`); the caller
/// uses `env.meta` to know which paths to revive in V8 land. Returning a
/// `Result` keeps the door open for future shadow-validation passes (e.g.
/// rejecting tagged paths that don't resolve in `env.json`).
///
/// # Errors
///
/// Currently infallible; the `Result` return shape is reserved for future
/// validation as documented above.
pub fn decode(env: Envelope) -> Result<Value, Error> {
    Ok(env.json)
}

/// Serialize an envelope to canonical wire bytes.
///
/// Empty meta is omitted entirely, matching npm `superjson` for pure-JSON
/// payloads (the 80% case). Field order matches npm — `json` before `meta`,
/// and inside `meta` it's `values` before `v` — because byte-equality with
/// the reference implementation is the contract.
#[must_use]
pub fn to_bytes(env: &Envelope) -> Vec<u8> {
    let mut buf = Vec::with_capacity(64);
    buf.push(b'{');
    buf.extend_from_slice(b"\"json\":");
    serde_json::to_writer(&mut buf, &env.json)
        .expect("Value serialization is infallible for owned Value tree");
    if let Some(meta) = &env.meta {
        if !meta.is_empty() {
            let values = meta_values_to_json(meta);
            buf.extend_from_slice(b",\"meta\":{\"values\":");
            serde_json::to_writer(&mut buf, &values)
                .expect("Value serialization is infallible for owned Value tree");
            buf.extend_from_slice(b",\"v\":1}");
        }
    }
    buf.push(b'}');
    buf
}

/// Parse wire bytes back to an envelope.
///
/// Accepts both the `{json, meta}` shape and a bare value (for legacy
/// clients that don't wrap their payload). Bare values become
/// `Envelope { json: v, meta: None }`.
///
/// # Errors
///
/// Returns `Error::InvalidJson` if the bytes aren't valid JSON,
/// `Error::MalformedMeta` if the meta envelope is structurally wrong,
/// and `Error::UnknownTag` if the wire references a tag this build
/// doesn't recognise.
pub fn from_bytes(bytes: &[u8]) -> Result<Envelope, Error> {
    let raw: Value =
        serde_json::from_slice(bytes).map_err(|e| Error::InvalidJson(e.to_string()))?;
    parse_envelope(raw)
}

// ---------------------------------------------------------------------------
// Decoding (wire → typed Meta)
// ---------------------------------------------------------------------------

/// Distinguishing a wrapped envelope from a bare legacy value: the wrapper
/// MUST be an object containing a `"json"` key. Anything else is bare.
fn parse_envelope(raw: Value) -> Result<Envelope, Error> {
    let Value::Object(mut obj) = raw else {
        return Ok(Envelope { json: raw, meta: None });
    };
    let Some(json) = obj.remove("json") else {
        // Bare object (legacy path) — round-trip as-is.
        return Ok(Envelope { json: Value::Object(obj), meta: None });
    };
    let meta = match obj.remove("meta") {
        None => None,
        Some(Value::Object(mut meta_obj)) => {
            // The `v` field is informational; we accept any value but
            // currently only `v: 1` is in the wild.
            meta_obj.remove("v");
            let values = meta_obj
                .remove("values")
                .ok_or_else(|| Error::MalformedMeta("`values` missing".into()))?;
            Some(parse_meta_values(values)?)
        }
        Some(_) => return Err(Error::MalformedMeta("`meta` must be an object".into())),
    };
    Ok(Envelope { json, meta })
}

/// `meta.values` may be:
///   * an object of `path → tag`   (root is plain, has rich descendants),
///   * a tag (any form)            (root is rich; possibly with children).
fn parse_meta_values(v: Value) -> Result<Meta, Error> {
    match v {
        Value::Object(map) => {
            let mut values = IndexMap::new();
            for (path, tag_v) in map {
                values.insert(path, parse_tag(tag_v)?);
            }
            Ok(Meta { root: None, values })
        }
        arr @ Value::Array(_) => Ok(Meta {
            root: Some(parse_tag(arr)?),
            values: IndexMap::new(),
        }),
        other => Err(Error::MalformedMeta(format!(
            "expected object or array, got {other:?}"
        ))),
    }
}

/// A tag is always wire-encoded as a JSON array `[head, ?children]` where
/// `head` is either a primitive marker (`"Date"`, `"map"`, `"number"`, …)
/// or a `["typed-array", "<Ctor>"]` sub-tuple.
fn parse_tag(v: Value) -> Result<MetaTag, Error> {
    let Value::Array(arr) = v else {
        return Err(Error::MalformedMeta(format!("tag must be array, got {v:?}")));
    };
    if arr.is_empty() {
        return Err(Error::MalformedMeta("empty tag array".into()));
    }
    let mut iter = arr.into_iter();
    let head = iter.next().unwrap();
    let inner = parse_tag_head(head)?;
    let Some(rest) = iter.next() else {
        return Ok(inner);
    };
    let Value::Object(children_map) = rest else {
        return Err(Error::MalformedMeta(format!(
            "tag children must be object, got {rest:?}"
        )));
    };
    let mut children = IndexMap::new();
    for (path, tag_v) in children_map {
        children.insert(path, parse_tag(tag_v)?);
    }
    Ok(MetaTag::WithChildren(Box::new(inner), children))
}

/// Decode the head element of a tag: a string marker or a typed-array tuple.
fn parse_tag_head(v: Value) -> Result<MetaTag, Error> {
    match v {
        Value::String(s) => match s.as_str() {
            "Date" => Ok(MetaTag::Date),
            "bigint" => Ok(MetaTag::BigInt),
            "map" => Ok(MetaTag::Map),
            "set" => Ok(MetaTag::Set),
            "regexp" => Ok(MetaTag::Regexp),
            "URL" => Ok(MetaTag::Url),
            "undefined" => Ok(MetaTag::Undefined),
            "number" => Ok(MetaTag::Number),
            other => Err(Error::UnknownTag(other.to_string())),
        },
        Value::Array(arr) => {
            // Only known nested-head form is ["typed-array", "<Ctor>"].
            let head = arr
                .first()
                .and_then(Value::as_str)
                .ok_or_else(|| Error::MalformedMeta("empty nested tag head".into()))?;
            if head != "typed-array" {
                return Err(Error::UnknownTag(head.to_string()));
            }
            let ctor = arr
                .get(1)
                .and_then(Value::as_str)
                .ok_or_else(|| {
                    Error::MalformedMeta("typed-array tag missing constructor name".into())
                })?
                .to_string();
            Ok(MetaTag::TypedArray(ctor))
        }
        other => Err(Error::MalformedMeta(format!(
            "tag head must be string or array, got {other:?}"
        ))),
    }
}

// ---------------------------------------------------------------------------
// Encoding (typed Meta → wire)
// ---------------------------------------------------------------------------

fn meta_values_to_json(meta: &Meta) -> Value {
    if let Some(root) = &meta.root {
        return tag_to_json(root);
    }
    // Path-keyed object form.
    let mut obj = JsonMap::new();
    for (path, tag) in &meta.values {
        obj.insert(path.clone(), tag_to_json(tag));
    }
    Value::Object(obj)
}

/// Encode a tag as its wire array. Simple tags become 1-element arrays;
/// `WithChildren` becomes a 2-element `[head, children]` array.
fn tag_to_json(tag: &MetaTag) -> Value {
    match tag {
        MetaTag::WithChildren(inner, children) => {
            let head = tag_head_value(inner);
            let mut child_obj = JsonMap::new();
            for (path, child_tag) in children {
                child_obj.insert(path.clone(), tag_to_json(child_tag));
            }
            Value::Array(vec![head, Value::Object(child_obj)])
        }
        other => Value::Array(vec![tag_head_value(other)]),
    }
}

/// The head of a tag — string marker for primitive tags, `["typed-array",
/// "<Ctor>"]` for typed arrays. `WithChildren` recursively unwraps to its
/// inner tag's head; the encoder lifts the children object out separately.
fn tag_head_value(tag: &MetaTag) -> Value {
    match tag {
        MetaTag::Date => json!("Date"),
        MetaTag::BigInt => json!("bigint"),
        MetaTag::Map => json!("map"),
        MetaTag::Set => json!("set"),
        MetaTag::Regexp => json!("regexp"),
        MetaTag::Url => json!("URL"),
        MetaTag::Undefined => json!("undefined"),
        MetaTag::Number => json!("number"),
        MetaTag::TypedArray(ctor) => json!(["typed-array", ctor]),
        // Nested WithChildren has no canonical head — the wire format never
        // produces it, but we degrade gracefully by recursing.
        MetaTag::WithChildren(inner, _) => tag_head_value(inner),
    }
}
