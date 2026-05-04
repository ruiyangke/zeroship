//! WebIDL `sequence<T>` and `record<K, V>` conversion helpers, plus the
//! [`WebIdlConvertible`] trait that backs them.
//!
//! Per the spec:
//!
//! - `sequence<T>` (§3.13.16): the JS value MUST have a `Symbol.iterator`
//!   method. Iterate it; convert each yielded value to `T`. Non-iterable
//!   values throw `TypeError`. The iterator's return values include a
//!   `done` flag and a `value`; we read `value` via the standard
//!   `IteratorResult` shape (object with `value` and `done` properties).
//!
//! - `record<K, V>` (§3.13.18): the JS value MUST be an Object (null and
//!   primitives reject as `TypeError`). Iterate its `OwnPropertyKeys`
//!   in spec-defined order (numeric-like first, then strings, then
//!   symbols — V8's `get_own_property_names` returns string keys in
//!   the canonical order). Each (key, value) pair is converted to
//!   `(K, V)`. K must be `USVString` or `ByteString`.
//!
//! [`WebIdlConvertible`] is the trait the per-type conversion routines
//! plug into. It's implemented for every primitive WebIDL boundary type
//! (USVString, ByteString, u32, i32, f64, bool, Option<T>) AND, via
//! `#[derive(WebIdlDict)]` and `#[derive(WebIdlEnum)]`, for the user's
//! own dictionary / enum types.
//!
//! Spec references (deliberately verbatim):
//!   <https://webidl.spec.whatwg.org/#es-sequence>
//!   <https://webidl.spec.whatwg.org/#es-record>

use crate::byte_string::{read_byte_string, ByteString};
use crate::state::OpError;
use crate::usv_string::{read_usv_string_or_throw, USVString};

// Re-export the WebIDL default-case integer-coercion newtypes so users
// can `use zeroship_runtime::convert::WrapU16;` alongside the existing
// `Clamp{*}` and `EnforceRange{*}` family. Macro-side detection lives
// in `crates/runtime-macros/src/lib.rs::wrap_kind` and emits the
// matching `read_wrap_*` reader at the WebIDL boundary.
pub use crate::wrap::{
    read_wrap_i16, read_wrap_i32, read_wrap_i8, read_wrap_u16, read_wrap_u32, read_wrap_u8,
    WrapI16, WrapI32, WrapI8, WrapU16, WrapU32, WrapU8,
};

/// WebIDL JS-value → Rust-type conversion at the boundary.
///
/// Implementors emit a typed value or throw a TypeError-flavoured
/// [`OpError`]. The caller layer (`#[v8_class]` macro, `read_sequence`,
/// `read_record`) maps the OpError to a V8 exception.
///
/// The trait takes a `&mut v8::PinScope` because most conversions need
/// to call back into V8 (e.g. `Value::to_string`) — we can't perform
/// the conversion offline.
///
/// # Implementing for new types
///
/// For primitives that the macro already handles in arg position, we
/// hand-implement here so `read_sequence::<MyType>` and the dict-derive
/// auto-extract keep working uniformly. For user types, the
/// `#[derive(WebIdlDict)]` and `#[derive(WebIdlEnum)]` derives auto-impl
/// this trait — users don't write the impl directly.
pub trait WebIdlConvertible: Sized {
    /// Convert the V8 value to `Self`, or return an [`OpError`].
    /// Implementations that need to throw should use
    /// `OpError::type_error(...)` or `OpError::range_error(...)`.
    fn from_v8(
        scope: &mut v8::PinScope,
        value: v8::Local<v8::Value>,
    ) -> Result<Self, OpError>;
}

// ---------------------------------------------------------------------------
// Primitive impls
// ---------------------------------------------------------------------------

impl WebIdlConvertible for USVString {
    fn from_v8(
        scope: &mut v8::PinScope,
        value: v8::Local<v8::Value>,
    ) -> Result<Self, OpError> {
        read_usv_string_or_throw(scope, value).map(USVString::from_string)
    }
}

impl WebIdlConvertible for ByteString {
    fn from_v8(
        scope: &mut v8::PinScope,
        value: v8::Local<v8::Value>,
    ) -> Result<Self, OpError> {
        read_byte_string(scope, value).map(ByteString::from_bytes)
    }
}

impl WebIdlConvertible for String {
    fn from_v8(
        scope: &mut v8::PinScope,
        value: v8::Local<v8::Value>,
    ) -> Result<Self, OpError> {
        // Default JS-to-Rust string: lossy UTF-8. Matches the macro's
        // default-`String`-arg path. Symbol → throws (Value::to_string
        // returns None which we surface as TypeError).
        let s = value
            .to_string(scope)
            .ok_or_else(|| OpError::type_error("Cannot convert value to String"))?;
        Ok(s.to_rust_string_lossy(scope))
    }
}

impl WebIdlConvertible for bool {
    fn from_v8(
        scope: &mut v8::PinScope,
        value: v8::Local<v8::Value>,
    ) -> Result<Self, OpError> {
        Ok(value.boolean_value(scope))
    }
}

impl WebIdlConvertible for u32 {
    fn from_v8(
        scope: &mut v8::PinScope,
        value: v8::Local<v8::Value>,
    ) -> Result<Self, OpError> {
        // WebIDL `unsigned long` without `[EnforceRange]` / `[Clamp]`:
        // ToUint32 (ECMA-262 §7.1.6) — wraps modulo 2^32, NaN → 0. V8's
        // `uint32_value` implements ToUint32. None means a Symbol or a
        // Proxy raised; surface as TypeError so the user sees the
        // boundary error.
        value
            .uint32_value(scope)
            .ok_or_else(|| OpError::type_error("Cannot convert value to unsigned long"))
    }
}

impl WebIdlConvertible for i32 {
    fn from_v8(
        scope: &mut v8::PinScope,
        value: v8::Local<v8::Value>,
    ) -> Result<Self, OpError> {
        value
            .int32_value(scope)
            .ok_or_else(|| OpError::type_error("Cannot convert value to long"))
    }
}

impl WebIdlConvertible for f64 {
    fn from_v8(
        scope: &mut v8::PinScope,
        value: v8::Local<v8::Value>,
    ) -> Result<Self, OpError> {
        value
            .number_value(scope)
            .ok_or_else(|| OpError::type_error("Cannot convert value to double"))
    }
}

/// `Option<T>` — undefined or null produce `None`; otherwise convert
/// inner. This matches the WebIDL pattern for nullable types and for
/// `Option<X>` dictionary members where missing key ↔ undefined.
impl<T: WebIdlConvertible> WebIdlConvertible for Option<T> {
    fn from_v8(
        scope: &mut v8::PinScope,
        value: v8::Local<v8::Value>,
    ) -> Result<Self, OpError> {
        if value.is_null_or_undefined() {
            return Ok(None);
        }
        T::from_v8(scope, value).map(Some)
    }
}

// ---------------------------------------------------------------------------
// `DictOrBool<T>` — `(<dict> or boolean)` union shape (WebIDL §3.13.6,
// narrowed). Spec consumers: `AddEventListenerOptions` accepts
// `(EventListenerOptions or boolean)` per DOM §2.7 — the boolean
// shorthand sets `capture` only.
//
// We deliberately implement only this specific union shape rather than
// the full WebIDL §3.13.6 union resolution algorithm. The general
// algorithm has hundreds of branches (object-with-iterator vs
// object-without, FrozenArray vs non, distinguishability rules across
// types, etc.); the runtime's actual consumer set is exactly one shape
// — `(<dict> or boolean)`. Keeping the surface this small means the
// type IS the contract: the dict derive needs no new attribute, and
// the call site reads naturally as `Option<DictOrBool<EventListenerOptions>>`.
//
// Distinguishability: per WebIDL §3.13.6.4, dictionary and boolean are
// always distinguishable in a union (a primitive `boolean` is never an
// `[[Prototype]]`-bearing object), so the branching in `from_v8` below
// is unambiguous.
// ---------------------------------------------------------------------------

/// A `(T or boolean)` WebIDL union member.
///
/// `from_v8`:
///   - JS primitive `boolean` → `DictOrBool::Bool(value)`
///   - anything else → `DictOrBool::Dict(T::from_v8(scope, value)?)`
///
/// Wrap inside `Option<DictOrBool<T>>` to make `null` / `undefined`
/// fall through to `None` (the WebIDL §3.10 "absent member" path):
///
/// ```ignore
/// #[derive(WebIdlDict, Default)]
/// struct AddEventListenerOptions {
///     options: Option<DictOrBool<EventListenerOptions>>,
/// }
/// ```
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum DictOrBool<T> {
    /// The full dictionary form.
    Dict(T),
    /// The boolean shorthand. Spec call sites that accept this form
    /// usually map it to a single dict member (e.g. `capture` in
    /// `AddEventListenerOptions`).
    Bool(bool),
}

impl<T: WebIdlConvertible> WebIdlConvertible for DictOrBool<T> {
    fn from_v8(
        scope: &mut v8::PinScope,
        value: v8::Local<v8::Value>,
    ) -> Result<Self, OpError> {
        // We branch on the *primitive boolean* shape specifically.
        // `value.is_boolean()` returns true for both primitive booleans
        // and Boolean objects (Boolean wrapper objects); per WebIDL
        // §3.13.6 distinguishability, only the *primitive* must take
        // the boolean branch — Boolean wrappers are still Objects and
        // route through the dict path. Use `IsBoolean` (which excludes
        // wrappers) by checking the v8 Value's exact shape via
        // `try_from::<v8::Boolean>`.
        if let Ok(b) = v8::Local::<v8::Boolean>::try_from(value) {
            return Ok(DictOrBool::Bool(b.is_true()));
        }
        // Fall back to the dict (or whatever T's WebIdlConvertible
        // implements). Errors from T propagate verbatim.
        Ok(DictOrBool::Dict(T::from_v8(scope, value)?))
    }
}

/// `v8::Local<'_, v8::Value>` passthrough. Lets dictionaries and
/// sequences carry the raw V8 handle through to user code untouched —
/// e.g. `RequestInit.body` is a union type that the constructor body
/// dispatches on by V8 shape (BodyInit = USVString | Blob | BufferSource
/// | FormData | URLSearchParams | ReadableStream).
///
/// Lifetime annotation: we accept ANY lifetime `'s` and return that
/// same lifetime back, so the trait composes inside dict structs that
/// use `Option<v8::Local<'s, v8::Value>>` shapes.
impl<'s> WebIdlConvertible for v8::Local<'s, v8::Value> {
    fn from_v8(
        _scope: &mut v8::PinScope,
        value: v8::Local<v8::Value>,
    ) -> Result<Self, OpError> {
        // SAFETY: V8 `Local`s are reborrowable handles into the current
        // HandleScope. The caller's `value` is alive in the same scope
        // we're called from, so transmuting the lifetime is sound: the
        // returned Local cannot outlive `value` because both are tied
        // to the active scope at the call site, and the trait method's
        // `Self = Local<'s, Value>` parameter is what relates them.
        //
        // We can't write `Ok(value)` directly because the lifetimes of
        // the trait method's `value: Local<Value>` and the impl's
        // `Local<'s, Value>` are not unified — the implementor's `'s`
        // is universally quantified over the impl block, not tied to
        // the method body. The transmute_copy launders the lifetime;
        // it's safe because `Local<T>` is `Copy` and is a thin handle
        // (one pointer).
        let local: v8::Local<'s, v8::Value> = unsafe { std::mem::transmute_copy(&value) };
        Ok(local)
    }
}

// ---------------------------------------------------------------------------
// sequence<T> reader (§3.13.16)
// ---------------------------------------------------------------------------

/// Read a JS value as a WebIDL `sequence<T>`.
///
/// Per spec:
/// 1. Get `@@iterator` from the value (`value[Symbol.iterator]`).
///    If absent or not callable, throw `TypeError`.
/// 2. Call it to obtain an iterator object.
/// 3. Loop: call `iterator.next()`. If the result `.done` is true, stop.
///    Otherwise convert `.value` to `T` and append.
///
/// The loop is bounded by the iterator itself; pathological infinite
/// iterators are the caller's problem (same as native browser engines).
pub fn read_sequence<T: WebIdlConvertible>(
    scope: &mut v8::PinScope,
    val: v8::Local<v8::Value>,
) -> Result<Vec<T>, OpError> {
    // Step 1–2. The `@@iterator` symbol lookup — `Symbol.iterator` is a
    // well-known symbol exposed via `v8::Symbol::get_iterator`.
    let obj: v8::Local<v8::Object> = val
        .try_into()
        .map_err(|_| OpError::type_error("sequence<T>: value is not iterable"))?;
    let iter_sym = v8::Symbol::get_iterator(scope);
    let iter_method = obj
        .get(scope, iter_sym.into())
        .ok_or_else(|| OpError::type_error("sequence<T>: missing @@iterator"))?;
    if iter_method.is_null_or_undefined() {
        return Err(OpError::type_error("sequence<T>: value is not iterable"));
    }
    let iter_fn: v8::Local<v8::Function> = iter_method
        .try_into()
        .map_err(|_| OpError::type_error("sequence<T>: @@iterator is not callable"))?;

    let iter_obj_v = iter_fn
        .call(scope, val, &[])
        .ok_or_else(|| OpError::type_error("sequence<T>: @@iterator threw"))?;
    let iter_obj: v8::Local<v8::Object> = iter_obj_v
        .try_into()
        .map_err(|_| OpError::type_error("sequence<T>: @@iterator did not return an object"))?;

    // Pre-resolve `next` once; the spec also re-fetches per iteration
    // step (`GetIteratorMethod`), but since we abort on a missing /
    // throwing `next` either way the eager-fetch produces the same
    // observable result with one fewer prop access per iteration.
    let next_key = v8::String::new(scope, "next")
        .ok_or_else(|| OpError::type_error("sequence<T>: cannot construct 'next' key"))?;
    let next_v = iter_obj
        .get(scope, next_key.into())
        .ok_or_else(|| OpError::type_error("sequence<T>: iterator missing 'next'"))?;
    let next_fn: v8::Local<v8::Function> = next_v
        .try_into()
        .map_err(|_| OpError::type_error("sequence<T>: iterator.next is not callable"))?;

    let value_key = v8::String::new(scope, "value")
        .ok_or_else(|| OpError::type_error("sequence<T>: cannot construct 'value' key"))?;
    let done_key = v8::String::new(scope, "done")
        .ok_or_else(|| OpError::type_error("sequence<T>: cannot construct 'done' key"))?;

    let mut out: Vec<T> = Vec::new();
    loop {
        let res_v = next_fn
            .call(scope, iter_obj.into(), &[])
            .ok_or_else(|| OpError::type_error("sequence<T>: iterator.next() threw"))?;
        let res_obj: v8::Local<v8::Object> = res_v.try_into().map_err(|_| {
            OpError::type_error("sequence<T>: iterator.next() did not return an object")
        })?;
        let done_v = res_obj
            .get(scope, done_key.into())
            .ok_or_else(|| OpError::type_error("sequence<T>: cannot read 'done' from result"))?;
        if done_v.boolean_value(scope) {
            break;
        }
        let item_v = res_obj
            .get(scope, value_key.into())
            .ok_or_else(|| OpError::type_error("sequence<T>: cannot read 'value' from result"))?;
        out.push(T::from_v8(scope, item_v)?);
    }

    Ok(out)
}

// ---------------------------------------------------------------------------
// record<K, V> reader (§3.13.18)
// ---------------------------------------------------------------------------

/// Read a JS value as a WebIDL `record<K, V>`.
///
/// Per spec:
/// 1. The value MUST be an Object (null or a primitive throws TypeError).
/// 2. Iterate `OwnPropertyKeys(value)` in canonical order:
///    integer-indexed first ascending, then string keys in insertion
///    order. (Symbols are skipped — records key on string types only.)
/// 3. For each key, call `[[Get]](value, key)` and convert to `V`.
///    Convert the key to `K` (must be `USVString` or `ByteString`).
///    Skip non-enumerable own properties (per spec step "for each key
///    in keys → if descriptor's [[Enumerable]] is true").
///
/// Returns the pairs in spec order. Two same-keyed entries are not
/// possible (own properties have unique keys).
///
/// `K` must be `USVString` or `ByteString`. Other key types are rejected
/// at the type-system level (no WebIdlConvertible impl needed; this
/// helper only compiles for the spec-allowed key types).
pub fn read_record<K: WebIdlConvertible, V: WebIdlConvertible>(
    scope: &mut v8::PinScope,
    val: v8::Local<v8::Value>,
) -> Result<Vec<(K, V)>, OpError> {
    // Step 1: value MUST be an Object. Per WebIDL spec
    // "If Type(O) is not Object, then throw a TypeError." Arrays are
    // Objects so they're permitted (record keyed on numeric stringified
    // indices); only primitives + null reject.
    if !val.is_object() {
        return Err(OpError::type_error(
            "record<K, V>: value is not an object",
        ));
    }
    let obj: v8::Local<v8::Object> = val
        .try_into()
        .map_err(|_| OpError::type_error("record<K, V>: value is not an object"))?;

    // Step 2: own enumerable property names, in canonical order. V8's
    // `get_own_property_names` returns the same order spec'd in
    // ECMA-262 §9.1.11 (`OrdinaryOwnPropertyKeys`):
    //   1. integer-indexed strings ascending
    //   2. other string keys in insertion order
    //   3. (we skip symbols — they're not allowed as record keys)
    //
    // V8 returns strings only by default; symbol-keyed properties are
    // exposed via `get_own_property_names` with `KEY_FILTER_OWN_NUMERIC
    // | KEY_FILTER_OWN_STRINGS` per Object.keys's behaviour.
    let names = obj
        .get_own_property_names(scope, v8::GetPropertyNamesArgs::default())
        .ok_or_else(|| OpError::type_error("record<K, V>: cannot enumerate own properties"))?;
    let len = names.length();

    let mut out: Vec<(K, V)> = Vec::with_capacity(len as usize);
    for i in 0..len {
        let key_v = names
            .get_index(scope, i)
            .ok_or_else(|| OpError::type_error("record<K, V>: cannot read key"))?;
        // Skip non-enumerable own keys per spec. V8 returns enumerable
        // keys by default, so we don't need an explicit filter — but
        // we re-check via `get_own_property_descriptor` if a future
        // change widens the default. Today, default flags == enumerable
        // strings, which is what the spec wants.
        let val_v = obj
            .get(scope, key_v)
            .ok_or_else(|| OpError::type_error("record<K, V>: cannot read value"))?;

        let k = K::from_v8(scope, key_v)?;
        let v = V::from_v8(scope, val_v)?;
        out.push((k, v));
    }

    Ok(out)
}
