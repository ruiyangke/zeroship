//! V8-aware superjson encode/decode for RPC v2.
//!
//! `zeroship_core::superjson` provides the wire envelope
//! (`Envelope`, `Meta`, `MetaTag`) and the serializer that round-trips
//! against the npm `superjson@2.x` fixtures. This file is the V8 half:
//! walk a `v8::Local<v8::Value>` extracting rich-typed leaves into the
//! meta map, and revive a wire envelope back to a V8 value.
//!
//! ## Type detection at encode time
//!
//! | V8 predicate | json shadow | meta tag |
//! |---|---|---|
//! | `is_date` | ISO-8601 string | `Date` |
//! | `is_big_int` | decimal string | `BigInt` |
//! | `is_map` | `[[k, v], ...]` array | `Map` (or `WithChildren`) |
//! | `is_set` | array | `Set` (or `WithChildren`) |
//! | `is_reg_exp` | `/source/flags` | `Regexp` |
//! | `URL::is_instance` | `href` string | `Url` |
//! | `is_uint8_array` | byte array | `TypedArray("Uint8Array")` |
//! | `is_undefined` | OMITTED (object) / `null` (root) | `Undefined` |
//! | `is_number && (NaN || ±Inf)` | sentinel string | `Number` |
//! | plain string/number/boolean/null | passthrough | (none) |
//! | plain object/array | recurse | (none) |
//!
//! ## Current limitations
//!
//! - Typed arrays: only `Uint8Array` round-trips. Other TypedArray
//!   constructors (`Int32Array`, `Float64Array`, etc.) are rejected at
//!   encode time with `OpError::type_error`. If needed, this can widen
//!   later to the full `["typed-array", "<Ctor>"]` family.
//! - Cycles: a self-referential object will exhaust the stack. The npm
//!   library doesn't handle cycles either — reasonable for an RPC
//!   wire format. Add an explicit `seen` set if the requirement changes.
//! - References: `superjson@2.x` has a `referentialEqualities` feature
//!   for repeated objects. We don't emit it (it's optional in the
//!   wire format).
//!
//! ## Path syntax
//!
//! Paths join with `.` — `a.b.0` is `obj.a.b[0]`. Map pair index `i`
//! contributes `i.0` (the key) and `i.1` (the value); Set element `i`
//! contributes `i`.

use indexmap::IndexMap;
use serde_json::{Map as JsonMap, Number, Value};
use zeroship_core::superjson::{Envelope, Meta, MetaTag};

use crate::state::OpError;
use crate::url_native::url::URL;

// ---------------------------------------------------------------------------
// Public API
// ---------------------------------------------------------------------------

/// Encode a V8 value to a wire envelope. Walks the value tree
/// extracting rich-typed leaves into `Envelope.meta`. The returned
/// envelope is what the gateway / npm `superjson` library would
/// produce given the same input.
pub fn encode_to_envelope<'s>(
    scope: &mut v8::PinScope<'s, '_>,
    value: v8::Local<'s, v8::Value>,
) -> Result<Envelope, OpError> {
    let mut visitor = EncodeVisitor::default();
    let json = visitor.encode_root(scope, value)?;
    let meta = visitor.finish();
    Ok(Envelope { json, meta })
}

/// Decode a wire envelope back into a V8 value. Walks `envelope.json`
/// revivifying rich types per `envelope.meta`.
///
/// Two distinct meta shapes drive different paths:
///   - **WithChildren(inner, kids)**: root is itself a rich container
///     (Set/Map). The kid paths are *internal* to the container —
///     `revive_set`/`revive_map` handle them inline so each Date/etc.
///     child is constructed at the correct time (Set elements need to
///     be Date instances when `set.add` runs, otherwise the Set sees a
///     plain string).
///   - **path-keyed values**: root is plain JSON; rich descendants live
///     at top-level walkable paths. `apply_child_paths` walks the
///     post-revival V8 tree replacing leaves.
pub fn decode_to_v8<'s>(
    scope: &mut v8::PinScope<'s, '_>,
    envelope: Envelope,
) -> Result<v8::Local<'s, v8::Value>, OpError> {
    let Envelope { json, meta } = envelope;
    let Some(meta) = meta else {
        return json_to_v8(scope, json);
    };
    if let Some(root_tag) = meta.root {
        // Root is rich — `revive` handles WithChildren internally for
        // Set / Map; for everything else WithChildren wouldn't have
        // children to apply (Date / BigInt / etc. are scalars).
        return revive(scope, json, Some(&root_tag));
    }
    // Root is plain JSON; descendants get path-keyed reviving.
    let revived = json_to_v8(scope, json)?;
    if meta.values.is_empty() {
        return Ok(revived);
    }
    apply_child_paths(scope, revived, &meta.values)
}

/// Convenience: encode a V8 value to wire bytes.
pub fn encode_to_bytes<'s>(
    scope: &mut v8::PinScope<'s, '_>,
    value: v8::Local<'s, v8::Value>,
) -> Result<Vec<u8>, OpError> {
    let env = encode_to_envelope(scope, value)?;
    Ok(zeroship_core::superjson::to_bytes(&env))
}

/// Convenience: parse wire bytes and revive a V8 value.
pub fn decode_from_bytes<'s>(
    scope: &mut v8::PinScope<'s, '_>,
    bytes: &[u8],
) -> Result<v8::Local<'s, v8::Value>, OpError> {
    let env = zeroship_core::superjson::from_bytes(bytes)
        .map_err(|e| OpError::type_error(format!("superjson: {e}")))?;
    decode_to_v8(scope, env)
}

// ---------------------------------------------------------------------------
// Encoder
// ---------------------------------------------------------------------------

/// Walks a V8 value, building both the JSON shadow (`Value`) and the
/// path → tag map (`IndexMap`). The visitor tracks the current path
/// stack so each rich-type leaf records its dot-joined path.
#[derive(Default)]
struct EncodeVisitor {
    /// Path → tag for non-root rich leaves.
    values: IndexMap<String, MetaTag>,
    /// Tag for the root if the root itself is rich.
    root: Option<MetaTag>,
    /// Working path stack — `current_path` joins this with `.`.
    path: Vec<String>,
}

impl EncodeVisitor {
    fn current_path(&self) -> String {
        self.path.join(".")
    }

    /// Record a tag for the current path. The root case (empty path
    /// stack) writes to `self.root`; nested paths go into `values`.
    fn record(&mut self, tag: MetaTag) {
        if self.path.is_empty() {
            self.root = Some(tag);
        } else {
            self.values.insert(self.current_path(), tag);
        }
    }

    fn finish(self) -> Option<Meta> {
        let meta = Meta {
            root: self.root,
            values: self.values,
        };
        if meta.is_empty() {
            None
        } else {
            Some(meta)
        }
    }

    fn encode_root<'s>(
        &mut self,
        scope: &mut v8::PinScope<'s, '_>,
        value: v8::Local<'s, v8::Value>,
    ) -> Result<Value, OpError> {
        self.encode(scope, value)
    }

    fn encode<'s>(
        &mut self,
        scope: &mut v8::PinScope<'s, '_>,
        value: v8::Local<'s, v8::Value>,
    ) -> Result<Value, OpError> {
        // 1. Primitive cases first — fast path that doesn't touch the
        // recursive object walker.
        if value.is_null() {
            return Ok(Value::Null);
        }
        if value.is_undefined() {
            // Top-level `undefined` round-trips as `json: null` + tag
            // `Undefined`. Inside an object, the caller handles the
            // omit-from-shadow rule before recursing here.
            self.record(MetaTag::Undefined);
            return Ok(Value::Null);
        }
        if value.is_boolean() {
            return Ok(Value::Bool(value.is_true()));
        }
        if value.is_number() {
            return Ok(encode_number(scope, value, |t| self.record(t)));
        }
        if value.is_string() {
            let s = value.to_rust_string_lossy(scope);
            return Ok(Value::String(s));
        }
        if value.is_big_int() {
            self.record(MetaTag::BigInt);
            // BigInt's ToString is the canonical decimal — ada/V8 don't
            // expose a direct method, so we route through the generic
            // Value::to_string which invokes BigInt.prototype.toString.
            let s = value.to_rust_string_lossy(scope);
            return Ok(Value::String(s));
        }

        // 2. Rich object types — date / regexp / typed-array / URL /
        // Map / Set. Order matters: URL brand-check before generic
        // is_object so we don't fall through to the dumb-object walker.
        if value.is_date() {
            self.record(MetaTag::Date);
            return Ok(encode_date(scope, value)?);
        }
        if value.is_reg_exp() {
            self.record(MetaTag::Regexp);
            return Ok(encode_regexp(scope, value)?);
        }
        if value.is_uint8_array() {
            self.record(MetaTag::TypedArray("Uint8Array".into()));
            return Ok(encode_uint8_array(scope, value)?);
        }
        // Reject other typed-array constructors. The current
        // implementation supports only Uint8Array.
        if is_other_typed_array(value) {
            return Err(OpError::type_error(
                "superjson encode: only Uint8Array typed-arrays are supported in phase 1",
            ));
        }
        if URL::is_instance(scope, value) {
            self.record(MetaTag::Url);
            // ToString on a URL instance hits URL.prototype.toString
            // which returns href.
            let href = value.to_rust_string_lossy(scope);
            return Ok(Value::String(href));
        }
        if value.is_map() {
            return self.encode_map(scope, value);
        }
        if value.is_set() {
            return self.encode_set(scope, value);
        }

        // 3. Generic object / array walk.
        if value.is_array() {
            return self.encode_array(scope, value);
        }
        if value.is_object() {
            return self.encode_plain_object(scope, value);
        }

        // 4. Fallback for symbols / functions / unrecognized — coerce
        // to null (matches what JSON.stringify does for these in object
        // values). Functions and symbols at the root produce `null`
        // with no meta — the npm superjson library does the same
        // implicit coercion.
        Ok(Value::Null)
    }

    fn encode_array<'s>(
        &mut self,
        scope: &mut v8::PinScope<'s, '_>,
        value: v8::Local<'s, v8::Value>,
    ) -> Result<Value, OpError> {
        let arr: v8::Local<v8::Array> = value
            .try_into()
            .map_err(|_| OpError::type_error("superjson encode: array cast failed"))?;
        let len = arr.length();
        let mut out = Vec::with_capacity(len as usize);
        for i in 0..len {
            let elem = arr.get_index(scope, i).ok_or_else(|| {
                OpError::type_error(format!("superjson encode: cannot read array[{i}]"))
            })?;
            self.path.push(i.to_string());
            // Arrays preserve `undefined` slots as `null` per JSON
            // (matching JSON.stringify), but we still tag them as
            // Undefined so the decoder restores the value.
            let v = if elem.is_undefined() {
                self.record(MetaTag::Undefined);
                Value::Null
            } else {
                self.encode(scope, elem)?
            };
            out.push(v);
            self.path.pop();
        }
        Ok(Value::Array(out))
    }

    fn encode_plain_object<'s>(
        &mut self,
        scope: &mut v8::PinScope<'s, '_>,
        value: v8::Local<'s, v8::Value>,
    ) -> Result<Value, OpError> {
        let obj: v8::Local<v8::Object> = value
            .try_into()
            .map_err(|_| OpError::type_error("superjson encode: object cast failed"))?;
        let names = obj
            .get_own_property_names(scope, v8::GetPropertyNamesArgs::default())
            .ok_or_else(|| OpError::type_error("superjson encode: cannot enumerate object"))?;
        let len = names.length();
        let mut out = JsonMap::new();
        for i in 0..len {
            let key_v = names.get_index(scope, i).ok_or_else(|| {
                OpError::type_error("superjson encode: cannot read property key")
            })?;
            let key = key_v.to_rust_string_lossy(scope);
            let val_v = obj
                .get(scope, key_v)
                .ok_or_else(|| OpError::type_error("superjson encode: cannot read property"))?;
            self.path.push(key.clone());
            // Per npm superjson: undefined VALUES of an object are
            // emitted as `null` in the JSON shadow (NOT omitted) and
            // tagged with `["undefined"]` in meta. The decoder explicitly
            // re-inserts them as `undefined`. Cross-checked against the
            // composite.json fixture's `"un":null` slot.
            if val_v.is_undefined() {
                self.record(MetaTag::Undefined);
                out.insert(key, Value::Null);
                self.path.pop();
                continue;
            }
            let v = self.encode(scope, val_v)?;
            out.insert(key, v);
            self.path.pop();
        }
        Ok(Value::Object(out))
    }

    fn encode_map<'s>(
        &mut self,
        scope: &mut v8::PinScope<'s, '_>,
        value: v8::Local<'s, v8::Value>,
    ) -> Result<Value, OpError> {
        let map: v8::Local<v8::Map> = value
            .try_into()
            .map_err(|_| OpError::type_error("superjson encode: Map cast failed"))?;
        // V8 returns key/value pairs as a flat array of length 2*size.
        let arr = map.as_array(scope);
        let len = arr.length();
        debug_assert_eq!(len % 2, 0);
        let pairs = (len / 2) as usize;
        // Switch into a child-collecting visitor: rich descendants of
        // the Map go into a `WithChildren` map, NOT the parent's
        // `values`.
        let saved_root = self.root.take();
        let saved_values = std::mem::take(&mut self.values);
        let saved_path = std::mem::take(&mut self.path);

        let mut out = Vec::with_capacity(pairs);
        for i in 0..pairs {
            let k = arr.get_index(scope, (i as u32) * 2).ok_or_else(|| {
                OpError::type_error("superjson encode: cannot read Map key")
            })?;
            let v = arr.get_index(scope, (i as u32) * 2 + 1).ok_or_else(|| {
                OpError::type_error("superjson encode: cannot read Map value")
            })?;
            self.path.push(format!("{i}.0"));
            let k_json = self.encode(scope, k)?;
            self.path.pop();
            self.path.push(format!("{i}.1"));
            let v_json = self.encode(scope, v)?;
            self.path.pop();
            out.push(Value::Array(vec![k_json, v_json]));
        }

        // Wrap up the children we collected and re-emit the Map tag for
        // the *outer* path level.
        let collected = std::mem::take(&mut self.values);
        // `root` should have remained None — the Map itself is the
        // node we're tagging now. Defensive restore in case a child
        // accidentally tagged root.
        self.root = saved_root;
        self.values = saved_values;
        self.path = saved_path;

        let tag = if collected.is_empty() {
            MetaTag::Map
        } else {
            MetaTag::WithChildren(Box::new(MetaTag::Map), collected)
        };
        self.record(tag);
        Ok(Value::Array(out))
    }

    fn encode_set<'s>(
        &mut self,
        scope: &mut v8::PinScope<'s, '_>,
        value: v8::Local<'s, v8::Value>,
    ) -> Result<Value, OpError> {
        let set: v8::Local<v8::Set> = value
            .try_into()
            .map_err(|_| OpError::type_error("superjson encode: Set cast failed"))?;
        let arr = set.as_array(scope);
        let len = arr.length();
        // Switch into a child-collecting visitor (see `encode_map`).
        let saved_root = self.root.take();
        let saved_values = std::mem::take(&mut self.values);
        let saved_path = std::mem::take(&mut self.path);

        let mut out = Vec::with_capacity(len as usize);
        for i in 0..len {
            let elem = arr.get_index(scope, i).ok_or_else(|| {
                OpError::type_error(format!("superjson encode: cannot read Set[{i}]"))
            })?;
            self.path.push(i.to_string());
            let v = self.encode(scope, elem)?;
            self.path.pop();
            out.push(v);
        }
        let collected = std::mem::take(&mut self.values);
        self.root = saved_root;
        self.values = saved_values;
        self.path = saved_path;

        let tag = if collected.is_empty() {
            MetaTag::Set
        } else {
            MetaTag::WithChildren(Box::new(MetaTag::Set), collected)
        };
        self.record(tag);
        Ok(Value::Array(out))
    }
}

/// Encode a V8 number — distinguishes plain finite values from
/// special floats (NaN / ±Infinity) which the npm wire tags as
/// `["number"]` and serializes as a sentinel string.
fn encode_number(
    scope: &mut v8::PinScope,
    value: v8::Local<v8::Value>,
    mut record_tag: impl FnMut(MetaTag),
) -> Value {
    let n = value.number_value(scope).expect("is_number => Number");
    if n.is_nan() {
        record_tag(MetaTag::Number);
        return Value::String("NaN".into());
    }
    if n.is_infinite() {
        record_tag(MetaTag::Number);
        return Value::String(if n > 0.0 { "Infinity".into() } else { "-Infinity".into() });
    }
    // Finite — round-trip via `serde_json::Number`. Integers stay as
    // ints when n is exactly an integer in i64 range.
    if let Some(i) = checked_i64(n) {
        return Value::Number(Number::from(i));
    }
    if let Some(num) = Number::from_f64(n) {
        return Value::Number(num);
    }
    // Should be unreachable for finite f64 — but keep a safe fallback.
    Value::Null
}

fn checked_i64(n: f64) -> Option<i64> {
    if n.fract() == 0.0 && n >= i64::MIN as f64 && n <= i64::MAX as f64 {
        let i = n as i64;
        if (i as f64) == n {
            return Some(i);
        }
    }
    None
}

/// `Date.prototype.toISOString()` via `Date::value_of()` (millisecond
/// epoch) — formatted to match the JS impl bit-for-bit.
fn encode_date<'s>(
    _scope: &mut v8::PinScope<'s, '_>,
    value: v8::Local<'s, v8::Value>,
) -> Result<Value, OpError> {
    // Cheaper than calling Date.prototype.toISOString through the JS
    // boundary: use V8's typed accessor + format manually.
    let date: v8::Local<v8::Date> = value
        .try_into()
        .map_err(|_| OpError::type_error("superjson encode: Date cast failed"))?;
    let ms = date.value_of();
    Ok(Value::String(format_iso_8601(ms)))
}

/// Format an epoch-ms timestamp as ISO-8601 with ms precision, matching
/// `Date.prototype.toISOString()` (`YYYY-MM-DDTHH:MM:SS.sssZ`).
///
/// Avoids pulling in a date library — the format is fixed and the
/// algorithm fits in <40 lines. Handles BCE dates if `ms` is negative
/// (JS allows years 0001-9999 from toISOString; outside that range it
/// throws RangeError, but Date itself supports the wider span — we
/// don't enforce the spec range here, simpler for the wire layer).
fn format_iso_8601(ms: f64) -> String {
    let total_ms = ms as i64;
    let secs = total_ms.div_euclid(1000);
    let millis = total_ms.rem_euclid(1000) as u32;
    // Days since unix epoch (1970-01-01 UTC).
    let days = secs.div_euclid(86400);
    let day_secs = secs.rem_euclid(86400) as u32;
    let h = day_secs / 3600;
    let m = (day_secs / 60) % 60;
    let s = day_secs % 60;

    // Civil-from-days algorithm by Howard Hinnant
    // (https://howardhinnant.github.io/date_algorithms.html#civil_from_days).
    let z = days + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z.rem_euclid(146_097); // [0, 146096]
    let yoe = (doe - doe / 1460 + doe / 36524 - doe / 146_096) / 365; // [0,399]
    let y = yoe + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100); // [0, 365]
    let mp = (5 * doy + 2) / 153; // [0, 11]
    let d = (doy - (153 * mp + 2) / 5 + 1) as u32;
    let mn = (mp + if mp < 10 { 3 } else { -9 }) as u32;
    let year = (y + if mn <= 2 { 1 } else { 0 }) as i64;

    format!(
        "{:04}-{:02}-{:02}T{:02}:{:02}:{:02}.{:03}Z",
        year, mn, d, h, m, s, millis
    )
}

/// `/source/flags` form, matching `RegExp.prototype.toString()`.
fn encode_regexp<'s>(
    scope: &mut v8::PinScope<'s, '_>,
    value: v8::Local<'s, v8::Value>,
) -> Result<Value, OpError> {
    // Easiest correct path: call `String(re)` which dispatches to
    // RegExp.prototype.toString → `/source/flags`. Rather than route
    // through JS, we read source + flags directly from the regex object
    // for a Rust-only path.
    let re: v8::Local<v8::RegExp> = value
        .try_into()
        .map_err(|_| OpError::type_error("superjson encode: RegExp cast failed"))?;
    let src = re.get_source(scope).to_rust_string_lossy(scope);
    // No native flags getter on v8::RegExp; ask JS via the `flags`
    // property (RegExp.prototype.flags is a spec-mandated string).
    let obj: v8::Local<v8::Object> = re.into();
    let flags_key = v8::String::new(scope, "flags").unwrap();
    let flags_v = obj
        .get(scope, flags_key.into())
        .ok_or_else(|| OpError::type_error("superjson encode: cannot read RegExp.flags"))?;
    let flags = flags_v.to_rust_string_lossy(scope);
    Ok(Value::String(format!("/{src}/{flags}")))
}

fn encode_uint8_array<'s>(
    _scope: &mut v8::PinScope<'s, '_>,
    value: v8::Local<'s, v8::Value>,
) -> Result<Value, OpError> {
    let view: v8::Local<v8::ArrayBufferView> = value
        .try_into()
        .map_err(|_| OpError::type_error("superjson encode: Uint8Array cast failed"))?;
    let n = view.byte_length();
    let mut buf = vec![0u8; n];
    if n > 0 {
        view.copy_contents(&mut buf);
    }
    Ok(Value::Array(
        buf.into_iter()
            .map(|b| Value::Number(Number::from(b)))
            .collect(),
    ))
}

/// True when the value is a typed-array view we DON'T support yet.
/// We check `is_array_buffer_view()` and exclude `is_uint8_array` /
/// `is_data_view` / `is_array_buffer` — the latter two aren't typed
/// arrays so they should fall through to the generic object walker.
fn is_other_typed_array(value: v8::Local<v8::Value>) -> bool {
    if value.is_uint8_array() {
        return false;
    }
    // Anything else with the typed-array brand. Add helpers from v8
    // crate: int8/uint8clamped/int16/uint16/int32/uint32/float32/
    // float64/bigint64/biguint64.
    value.is_int8_array()
        || value.is_uint8_clamped_array()
        || value.is_int16_array()
        || value.is_uint16_array()
        || value.is_int32_array()
        || value.is_uint32_array()
        || value.is_float32_array()
        || value.is_float64_array()
        || value.is_big_int64_array()
        || value.is_big_uint64_array()
}

// ---------------------------------------------------------------------------
// Decoder — Envelope ⇒ V8
// ---------------------------------------------------------------------------

fn revive<'s>(
    scope: &mut v8::PinScope<'s, '_>,
    json: Value,
    tag: Option<&MetaTag>,
) -> Result<v8::Local<'s, v8::Value>, OpError> {
    match tag {
        None => json_to_v8(scope, json),
        Some(MetaTag::Date) => revive_date(scope, json),
        Some(MetaTag::BigInt) => revive_bigint(scope, json),
        Some(MetaTag::Map) => revive_map(scope, json, &IndexMap::new()),
        Some(MetaTag::Set) => revive_set(scope, json, &IndexMap::new()),
        Some(MetaTag::Regexp) => revive_regexp(scope, json),
        Some(MetaTag::Url) => revive_url(scope, json),
        Some(MetaTag::Undefined) => Ok(v8::undefined(scope).into()),
        Some(MetaTag::Number) => revive_special_number(scope, json),
        Some(MetaTag::TypedArray(ctor)) => revive_typed_array(scope, json, ctor),
        Some(MetaTag::WithChildren(inner, children)) => {
            // Container with rich descendants — revive the container,
            // then patch each child path.
            match inner.as_ref() {
                MetaTag::Map => revive_map(scope, json, children),
                MetaTag::Set => revive_set(scope, json, children),
                _ => {
                    // Other WithChildren containers aren't part of the
                    // wire vocabulary; revive plainly then patch.
                    let revived = json_to_v8(scope, json)?;
                    apply_child_paths(scope, revived, children)
                }
            }
        }
    }
}

/// Convert a `serde_json::Value` to its V8 counterpart. No rich-type
/// hydration here — the caller wires the tag map separately.
fn json_to_v8<'s>(
    scope: &mut v8::PinScope<'s, '_>,
    value: Value,
) -> Result<v8::Local<'s, v8::Value>, OpError> {
    Ok(match value {
        Value::Null => v8::null(scope).into(),
        Value::Bool(b) => v8::Boolean::new(scope, b).into(),
        Value::Number(n) => {
            if let Some(i) = n.as_i64() {
                v8::Number::new(scope, i as f64).into()
            } else if let Some(u) = n.as_u64() {
                v8::Number::new(scope, u as f64).into()
            } else if let Some(f) = n.as_f64() {
                v8::Number::new(scope, f).into()
            } else {
                v8::null(scope).into()
            }
        }
        Value::String(s) => v8::String::new(scope, &s)
            .ok_or_else(|| OpError::type_error("superjson decode: string allocation failed"))?
            .into(),
        Value::Array(arr) => {
            let v8_arr = v8::Array::new(scope, arr.len() as i32);
            for (i, elem) in arr.into_iter().enumerate() {
                let v = json_to_v8(scope, elem)?;
                v8_arr.set_index(scope, i as u32, v);
            }
            v8_arr.into()
        }
        Value::Object(map) => {
            let obj = v8::Object::new(scope);
            for (k, v) in map {
                let key = v8::String::new(scope, &k)
                    .ok_or_else(|| OpError::type_error("superjson decode: key alloc failed"))?;
                let val = json_to_v8(scope, v)?;
                obj.set(scope, key.into(), val);
            }
            obj.into()
        }
    })
}

fn revive_date<'s>(
    scope: &mut v8::PinScope<'s, '_>,
    json: Value,
) -> Result<v8::Local<'s, v8::Value>, OpError> {
    let s = match json {
        Value::String(s) => s,
        other => {
            return Err(OpError::type_error(format!(
                "superjson decode: Date payload must be string, got {other}",
            )))
        }
    };
    let ms = parse_iso_to_ms(&s)
        .ok_or_else(|| OpError::type_error(format!("superjson decode: invalid Date: {s}")))?;
    let date = v8::Date::new(scope, ms)
        .ok_or_else(|| OpError::type_error("superjson decode: Date construction failed"))?;
    Ok(date.into())
}

fn parse_iso_to_ms(s: &str) -> Option<f64> {
    // Hand-rolled parser for the canonical `YYYY-MM-DDTHH:MM:SS.sssZ`
    // shape we emit on encode. Accepts optional ms fraction (.sss) and
    // an optional `Z` suffix; permissive of `+00:00` offsets too.
    // Returns None for anything we can't recognise.
    let s = s.trim();
    let bytes = s.as_bytes();
    if bytes.len() < 19 {
        return None;
    }
    let read_int = |start: usize, len: usize| -> Option<i64> {
        let end = start + len;
        if end > s.len() {
            return None;
        }
        s[start..end].parse::<i64>().ok()
    };
    let year = read_int(0, 4)?;
    if bytes[4] != b'-' { return None; }
    let mon = read_int(5, 2)?;
    if bytes[7] != b'-' { return None; }
    let day = read_int(8, 2)?;
    if bytes[10] != b'T' && bytes[10] != b' ' { return None; }
    let h = read_int(11, 2)?;
    if bytes[13] != b':' { return None; }
    let m = read_int(14, 2)?;
    if bytes[16] != b':' { return None; }
    let sec = read_int(17, 2)?;
    let mut idx = 19;
    let mut ms = 0i64;
    if idx < bytes.len() && bytes[idx] == b'.' {
        idx += 1;
        let start = idx;
        while idx < bytes.len() && bytes[idx].is_ascii_digit() {
            idx += 1;
        }
        let frac = &s[start..idx];
        // Pad/truncate to 3 digits.
        let mut fnum: i64 = frac.parse().ok()?;
        let mut len = idx - start;
        while len < 3 {
            fnum *= 10;
            len += 1;
        }
        while len > 3 {
            fnum /= 10;
            len -= 1;
        }
        ms = fnum;
    }
    let mut tz_offset_min: i64 = 0;
    if idx < bytes.len() {
        match bytes[idx] {
            b'Z' => idx += 1,
            b'+' | b'-' => {
                let sign: i64 = if bytes[idx] == b'+' { 1 } else { -1 };
                idx += 1;
                let oh = read_int(idx, 2)?;
                idx += 2;
                if idx < bytes.len() && bytes[idx] == b':' {
                    idx += 1;
                }
                let om = read_int(idx, 2)?;
                idx += 2;
                tz_offset_min = sign * (oh * 60 + om);
            }
            _ => return None,
        }
    }
    if idx != bytes.len() {
        return None;
    }
    // Days from civil — Hinnant inverse.
    let yy = if mon <= 2 { year - 1 } else { year };
    let era = yy.div_euclid(400);
    let yoe = yy - era * 400;
    let doy = (153 * (mon + if mon > 2 { -3 } else { 9 }) + 2) / 5 + day - 1;
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy;
    let days = era * 146_097 + doe - 719_468;
    let total_secs = days * 86400 + h * 3600 + m * 60 + sec - tz_offset_min * 60;
    Some((total_secs * 1000 + ms) as f64)
}

fn revive_bigint<'s>(
    scope: &mut v8::PinScope<'s, '_>,
    json: Value,
) -> Result<v8::Local<'s, v8::Value>, OpError> {
    let s = match json {
        Value::String(s) => s,
        // BigInt may also be written numerically by some senders.
        Value::Number(n) => n.to_string(),
        other => {
            return Err(OpError::type_error(format!(
                "superjson decode: BigInt payload must be string, got {other}",
            )))
        }
    };
    let s = s.trim();
    if let Ok(i) = s.parse::<i64>() {
        return Ok(v8::BigInt::new_from_i64(scope, i).into());
    }
    if let Ok(u) = s.parse::<u64>() {
        return Ok(v8::BigInt::new_from_u64(scope, u).into());
    }
    // Arbitrary-precision: parse the decimal string into a sign + words
    // (little-endian u64 words).
    let (sign, mag) = if let Some(rest) = s.strip_prefix('-') {
        (true, rest)
    } else if let Some(rest) = s.strip_prefix('+') {
        (false, rest)
    } else {
        (false, s)
    };
    let words = decimal_str_to_le_u64s(mag).ok_or_else(|| {
        OpError::type_error(format!("superjson decode: invalid BigInt literal: {s}"))
    })?;
    let bi = v8::BigInt::new_from_words(scope, sign, &words)
        .ok_or_else(|| OpError::type_error("superjson decode: BigInt construction failed"))?;
    Ok(bi.into())
}

/// Convert a decimal magnitude string (no sign / no leading zeros) into
/// little-endian u64 words. None on parse failure (non-digit chars).
fn decimal_str_to_le_u64s(s: &str) -> Option<Vec<u64>> {
    if s.is_empty() {
        return None;
    }
    let mut digits: Vec<u8> = Vec::with_capacity(s.len());
    for c in s.chars() {
        if !c.is_ascii_digit() {
            return None;
        }
        digits.push((c as u8) - b'0');
    }
    let mut words: Vec<u64> = Vec::new();
    // Repeatedly divmod by 2^64 to extract little-endian words.
    while !digits.iter().all(|&d| d == 0) {
        let mut carry: u128 = 0;
        let mut new_digits: Vec<u8> = Vec::with_capacity(digits.len());
        // Long division by 2^64 across the base-10 digit stream.
        let divisor: u128 = 1u128 << 64;
        for &d in &digits {
            carry = carry * 10 + d as u128;
            let q = carry / divisor;
            // q fits in a u8 only if 10*carry/2^64 < 10 — it does, since
            // carry < 2^64 going in, so 10*carry < 10*2^64 means q < 10.
            new_digits.push(q as u8);
            carry %= divisor;
        }
        words.push(carry as u64);
        // Drop leading zeros in the new quotient stream.
        let first_nonzero = new_digits.iter().position(|&d| d != 0).unwrap_or(new_digits.len());
        digits = new_digits.split_off(first_nonzero);
    }
    if words.is_empty() {
        words.push(0);
    }
    Some(words)
}

fn revive_map<'s>(
    scope: &mut v8::PinScope<'s, '_>,
    json: Value,
    children: &IndexMap<String, MetaTag>,
) -> Result<v8::Local<'s, v8::Value>, OpError> {
    let arr = match json {
        Value::Array(a) => a,
        other => {
            return Err(OpError::type_error(format!(
                "superjson decode: Map payload must be array, got {other}",
            )))
        }
    };
    let map = v8::Map::new(scope);
    for (i, pair) in arr.into_iter().enumerate() {
        let mut pair_arr = match pair {
            Value::Array(p) if p.len() == 2 => p,
            other => {
                return Err(OpError::type_error(format!(
                    "superjson decode: Map[{i}] must be 2-array, got {other}",
                )))
            }
        };
        let v_json = pair_arr.pop().unwrap();
        let k_json = pair_arr.pop().unwrap();
        let k_tag = children.get(&format!("{i}.0")).cloned();
        let v_tag = children.get(&format!("{i}.1")).cloned();
        let k = revive(scope, k_json, k_tag.as_ref())?;
        let v = revive(scope, v_json, v_tag.as_ref())?;
        map.set(scope, k, v);
    }
    Ok(map.into())
}

fn revive_set<'s>(
    scope: &mut v8::PinScope<'s, '_>,
    json: Value,
    children: &IndexMap<String, MetaTag>,
) -> Result<v8::Local<'s, v8::Value>, OpError> {
    let arr = match json {
        Value::Array(a) => a,
        other => {
            return Err(OpError::type_error(format!(
                "superjson decode: Set payload must be array, got {other}",
            )))
        }
    };
    let set = v8::Set::new(scope);
    for (i, elem) in arr.into_iter().enumerate() {
        let tag = children.get(&i.to_string()).cloned();
        let v = revive(scope, elem, tag.as_ref())?;
        set.add(scope, v);
    }
    Ok(set.into())
}

fn revive_regexp<'s>(
    scope: &mut v8::PinScope<'s, '_>,
    json: Value,
) -> Result<v8::Local<'s, v8::Value>, OpError> {
    let s = match json {
        Value::String(s) => s,
        other => {
            return Err(OpError::type_error(format!(
                "superjson decode: RegExp payload must be string, got {other}",
            )))
        }
    };
    // Parse `/source/flags`. The source may contain literal `/`; per
    // `RegExp.prototype.toString` those are escaped (`\/`), so the
    // last unescaped `/` separates source from flags.
    if !s.starts_with('/') {
        return Err(OpError::type_error(format!(
            "superjson decode: RegExp must start with '/', got {s}",
        )));
    }
    // Find the last `/` that isn't preceded by a backslash count of
    // odd length. Walk from the end.
    let bytes = s.as_bytes();
    let mut last_slash = None;
    let mut i = bytes.len();
    while i > 1 {
        i -= 1;
        if bytes[i] == b'/' {
            // Count preceding backslashes.
            let mut j = i;
            let mut bs = 0;
            while j > 1 && bytes[j - 1] == b'\\' {
                bs += 1;
                j -= 1;
            }
            if bs % 2 == 0 {
                last_slash = Some(i);
                break;
            }
        }
    }
    let split = last_slash.ok_or_else(|| {
        OpError::type_error(format!("superjson decode: malformed RegExp string: {s}"))
    })?;
    let src = &s[1..split];
    let flags_str = &s[split + 1..];
    let flags = parse_regex_flags(flags_str)
        .ok_or_else(|| OpError::type_error(format!("superjson decode: bad RegExp flags: {flags_str}")))?;
    let pat = v8::String::new(scope, src)
        .ok_or_else(|| OpError::type_error("superjson decode: RegExp source alloc failed"))?;
    let re = v8::RegExp::new(scope, pat, flags)
        .ok_or_else(|| OpError::type_error("superjson decode: RegExp::new failed"))?;
    Ok(re.into())
}

fn parse_regex_flags(s: &str) -> Option<v8::RegExpCreationFlags> {
    let mut f = v8::RegExpCreationFlags::empty();
    for c in s.chars() {
        match c {
            'g' => f |= v8::RegExpCreationFlags::GLOBAL,
            'i' => f |= v8::RegExpCreationFlags::IGNORE_CASE,
            'm' => f |= v8::RegExpCreationFlags::MULTILINE,
            'y' => f |= v8::RegExpCreationFlags::STICKY,
            'u' => f |= v8::RegExpCreationFlags::UNICODE,
            's' => f |= v8::RegExpCreationFlags::DOT_ALL,
            'd' => f |= v8::RegExpCreationFlags::HAS_INDICES,
            'v' => f |= v8::RegExpCreationFlags::UNICODE_SETS,
            _ => return None,
        }
    }
    Some(f)
}

fn revive_url<'s>(
    scope: &mut v8::PinScope<'s, '_>,
    json: Value,
) -> Result<v8::Local<'s, v8::Value>, OpError> {
    let s = match json {
        Value::String(s) => s,
        other => {
            return Err(OpError::type_error(format!(
                "superjson decode: URL payload must be string, got {other}",
            )))
        }
    };
    // Construct via `new URL(href)` over the installed class function so
    // the result has the full URL prototype chain. This requires the
    // URL class to be installed on the isolate (`url_native::install_globals`).
    // Pull the class function from the global — robust against having
    // or not having a `UrlNativeSlot`.
    let global = scope.get_current_context().global(scope);
    let key = v8::String::new(scope, "URL").unwrap();
    let url_v = global
        .get(scope, key.into())
        .ok_or_else(|| OpError::type_error("superjson decode: URL not on globalThis"))?;
    let url_fn: v8::Local<v8::Function> = url_v
        .try_into()
        .map_err(|_| OpError::type_error("superjson decode: globalThis.URL is not a function"))?;
    let arg = v8::String::new(scope, &s)
        .ok_or_else(|| OpError::type_error("superjson decode: URL arg alloc failed"))?;
    let inst = url_fn
        .new_instance(scope, &[arg.into()])
        .ok_or_else(|| OpError::type_error(format!("superjson decode: URL parse failed: {s}")))?;
    Ok(inst.into())
}

fn revive_typed_array<'s>(
    scope: &mut v8::PinScope<'s, '_>,
    json: Value,
    ctor: &str,
) -> Result<v8::Local<'s, v8::Value>, OpError> {
    if ctor != "Uint8Array" {
        return Err(OpError::type_error(format!(
            "superjson decode: only Uint8Array is supported in phase 1, got {ctor}",
        )));
    }
    let arr = match json {
        Value::Array(a) => a,
        other => {
            return Err(OpError::type_error(format!(
                "superjson decode: Uint8Array payload must be array, got {other}",
            )))
        }
    };
    let n = arr.len();
    let ab = v8::ArrayBuffer::new(scope, n);
    if n > 0 {
        let store = ab.get_backing_store();
        for (i, v) in arr.into_iter().enumerate() {
            let byte = v.as_u64().unwrap_or(0) as u8;
            store[i].set(byte);
        }
    }
    let u8 = v8::Uint8Array::new(scope, ab, 0, n)
        .ok_or_else(|| OpError::type_error("superjson decode: Uint8Array alloc failed"))?;
    Ok(u8.into())
}

fn revive_special_number<'s>(
    scope: &mut v8::PinScope<'s, '_>,
    json: Value,
) -> Result<v8::Local<'s, v8::Value>, OpError> {
    let s = match json {
        Value::String(s) => s,
        other => {
            return Err(OpError::type_error(format!(
                "superjson decode: number sentinel must be string, got {other}",
            )))
        }
    };
    let n = match s.as_str() {
        "Infinity" => f64::INFINITY,
        "-Infinity" => f64::NEG_INFINITY,
        "NaN" => f64::NAN,
        other => {
            return Err(OpError::type_error(format!(
                "superjson decode: unknown number sentinel: {other}",
            )))
        }
    };
    Ok(v8::Number::new(scope, n).into())
}

// ---------------------------------------------------------------------------
// Path patcher — for `WithChildren` containers and the
// path-keyed `meta.values` map at the root level. Walks `root` per the
// dot-joined path, replacing the leaf with a revived rich value.
// ---------------------------------------------------------------------------

fn apply_child_paths<'s>(
    scope: &mut v8::PinScope<'s, '_>,
    root: v8::Local<'s, v8::Value>,
    children: &IndexMap<String, MetaTag>,
) -> Result<v8::Local<'s, v8::Value>, OpError> {
    // The root value may itself be reassigned (top-level path "" maps
    // to root, but `WithChildren` never produces `""`-keyed
    // entries — paths always step into the container). Process longest-
    // first paths first to keep ancestor mutations from clobbering
    // descendant overwrites. In practice the npm wire emits paths in
    // top-down (shortest-first) order; we sort to be defensive.
    let mut keys: Vec<&String> = children.keys().collect();
    keys.sort_by_key(|k| k.matches('.').count()); // ascending depth
    for path in keys {
        let tag = children.get(path).expect("indexmap consistency");
        // Walk to the parent, then revive + assign the leaf.
        let segments: Vec<&str> = path.split('.').collect();
        if segments.is_empty() {
            continue;
        }
        let (last, prefix) = segments.split_last().unwrap();
        let mut cursor: v8::Local<'s, v8::Value> = root;
        for seg in prefix {
            cursor = step(scope, cursor, seg)?;
        }
        // Read the existing leaf to revive against; if that fails (the
        // path doesn't exist) skip the entry — the encoder is the only
        // wire-shape source so this should never happen, but cope with
        // foreign senders gracefully.
        let leaf = step(scope, cursor, last)?;
        // Convert leaf back to JSON, revive, and write.
        let leaf_json = v8_to_serde(scope, leaf);
        let revived = revive(scope, leaf_json, Some(tag))?;
        write_step(scope, cursor, last, revived)?;
    }
    Ok(root)
}

/// Step into a container by string segment — supports plain Object
/// keys and Array numeric indices. Returns the child value.
fn step<'s>(
    scope: &mut v8::PinScope<'s, '_>,
    parent: v8::Local<'s, v8::Value>,
    segment: &str,
) -> Result<v8::Local<'s, v8::Value>, OpError> {
    if parent.is_array() {
        let idx: u32 = segment
            .parse()
            .map_err(|_| OpError::type_error(format!("superjson decode: bad array index: {segment}")))?;
        let arr: v8::Local<v8::Array> = parent.try_into().unwrap();
        return arr
            .get_index(scope, idx)
            .ok_or_else(|| OpError::type_error(format!("superjson decode: array[{idx}] missing")));
    }
    if parent.is_object() {
        let obj: v8::Local<v8::Object> = parent
            .try_into()
            .map_err(|_| OpError::type_error("superjson decode: not an object"))?;
        let key = v8::String::new(scope, segment)
            .ok_or_else(|| OpError::type_error("superjson decode: key alloc failed"))?;
        return obj
            .get(scope, key.into())
            .ok_or_else(|| OpError::type_error(format!("superjson decode: key {segment:?} missing")));
    }
    Err(OpError::type_error(format!(
        "superjson decode: cannot step into non-container at segment {segment}",
    )))
}

/// Write a child segment back into its parent container.
fn write_step<'s>(
    scope: &mut v8::PinScope<'s, '_>,
    parent: v8::Local<'s, v8::Value>,
    segment: &str,
    value: v8::Local<'s, v8::Value>,
) -> Result<(), OpError> {
    if parent.is_array() {
        let idx: u32 = segment
            .parse()
            .map_err(|_| OpError::type_error(format!("superjson decode: bad array index: {segment}")))?;
        let arr: v8::Local<v8::Array> = parent.try_into().unwrap();
        arr.set_index(scope, idx, value);
        return Ok(());
    }
    if parent.is_object() {
        let obj: v8::Local<v8::Object> = parent
            .try_into()
            .map_err(|_| OpError::type_error("superjson decode: not an object"))?;
        let key = v8::String::new(scope, segment)
            .ok_or_else(|| OpError::type_error("superjson decode: key alloc failed"))?;
        obj.set(scope, key.into(), value);
        return Ok(());
    }
    Err(OpError::type_error(format!(
        "superjson decode: cannot write into non-container at segment {segment}",
    )))
}

/// Convert a V8 value back to `serde_json::Value` for re-revival on a
/// child path. Used when `apply_child_paths` finds a leaf that was
/// already revived as plain JSON (e.g. a Date string sitting at a
/// nested path) and needs to re-tag it.
fn v8_to_serde(scope: &mut v8::PinScope, v: v8::Local<v8::Value>) -> Value {
    if v.is_null() {
        return Value::Null;
    }
    if v.is_undefined() {
        return Value::Null;
    }
    if v.is_boolean() {
        return Value::Bool(v.is_true());
    }
    if v.is_number() {
        let n = v.number_value(scope).unwrap_or(0.0);
        if let Some(i) = checked_i64(n) {
            return Value::Number(Number::from(i));
        }
        if let Some(num) = Number::from_f64(n) {
            return Value::Number(num);
        }
        return Value::Null;
    }
    if v.is_string() {
        return Value::String(v.to_rust_string_lossy(scope));
    }
    if v.is_array() {
        let arr: v8::Local<v8::Array> = v.try_into().unwrap();
        let n = arr.length();
        let mut out = Vec::with_capacity(n as usize);
        for i in 0..n {
            let elem = arr.get_index(scope, i).unwrap_or_else(|| v8::null(scope).into());
            out.push(v8_to_serde(scope, elem));
        }
        return Value::Array(out);
    }
    if v.is_object() {
        let obj: v8::Local<v8::Object> = v.try_into().unwrap();
        if let Some(names) = obj.get_own_property_names(scope, v8::GetPropertyNamesArgs::default()) {
            let mut map = JsonMap::new();
            for i in 0..names.length() {
                let key_v = match names.get_index(scope, i) {
                    Some(k) => k,
                    None => continue,
                };
                let key = key_v.to_rust_string_lossy(scope);
                let val_v = match obj.get(scope, key_v) {
                    Some(v) => v,
                    None => continue,
                };
                map.insert(key, v8_to_serde(scope, val_v));
            }
            return Value::Object(map);
        }
    }
    Value::Null
}
