//! Native `Headers` per WHATWG Fetch §2.2 (https://fetch.spec.whatwg.org/#headers-class).
//!
//! Replaces the JS Headers polyfill that lived in `embed/fetch.js`,
//! which had several spec divergences (no ByteString validation, no
//! Set-Cookie special cases, name validation skipped on delete/has/get).
//!
//! Storage is `Vec<(Vec<u8>, Vec<u8>)>` — see "Storage" section of the
//! design doc for why bytes-not-strings and list-not-multimap. The list
//! is in insertion order; sort-and-combine runs lazily on iteration.
//!
//! ## Key correctness fixes vs the JS polyfill
//!
//! - **ByteString validation** (BLOCKER-2): code units > 0xFF throw
//!   TypeError before any storage write. The JS polyfill silently
//!   coerced via `String(value)` which lossily round-tripped non-Latin-1
//!   bytes.
//! - **Normalize-then-validate** (BLOCKER-3): Fetch §2.2.1 step 1 of
//!   `append`/`set` is *normalize value*, then validate. The polyfill
//!   validated the raw value, rejecting `"hello\r\n"` instead of
//!   stripping outer ws first.
//! - **Validate name on delete/has/get** (BLOCKER-4): Fetch §2.2.1
//!   step 1 of each calls validate(name, ""). The polyfill skipped
//!   this check, silently no-op'ing on bad names.
//! - **Live iteration** (BLOCKER-1): per WebIDL §3.7.10.2, iterator
//!   `next()` re-reads "value pairs to iterate over" on every call.
//!   Mutation between calls IS observable. The polyfill snapshotted
//!   keys at iterator construction.
//! - **Set-Cookie semantics** (PR #1346): `getSetCookie()` returns
//!   un-joined values; iteration emits one pair per set-cookie value;
//!   `get("set-cookie")` STILL JOINS with ", ".
//! - **Casing semantics**: at append time, reuse the casing of the
//!   first byte-case-insensitively-matching header currently in the
//!   list. After `delete`, casing resets — there's nothing in the
//!   list anymore. Iteration always emits lowercase.
//!
//! ## Forward-compat (Request/Response, [SameObject])
//!
//! Native Request/Response are coming. Spec marks `Request.headers` /
//! `Response.headers` as `[SameObject]`. v1 storage is owned by Headers
//! directly; the migration to shared ownership is a one-shot move of
//! `list`/`sorted_cache` into a `HeaderList` struct + `Rc<RefCell<…>>`
//! aliasing. Public algorithms here take `&mut self` / `&self` and
//! don't bake in single-owner assumptions.

use crate::byte_string::{read_byte_string, ByteString};
use crate::state::OpError;

#[allow(unused_imports)]
use zeroship_runtime_macros::{
    v8_class, v8_constructor, v8_getter, v8_inherit_intrinsic, v8_method, v8_name, v8_setter,
    v8_to_string_tag,
};

// ---------------------------------------------------------------------------
// Headers struct
// ---------------------------------------------------------------------------

/// Fetch §2.2 "headers guard". Per Fetch's `validate` algorithm:
///
///   * `None` (= Fetch's "none" guard): no restrictions.
///   * `Immutable` (= Fetch's "immutable" guard): every mutator throws
///     TypeError. Set on `Response.error()` headers per Fetch §6.2.4.
///
/// The `request` / `response` guards (forbidden-header-name filtering)
/// are deferred — see fetch-native v2 D-17.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum HeadersGuard {
    #[default]
    None,
    Immutable,
}

#[derive(Default)]
pub struct Headers {
    /// (name, value) byte pairs in insertion order. Names preserve the
    /// casing of the first byte-case-insensitively-matching header
    /// currently in the list per Fetch §2.2.1 `append` step 1.
    list: Vec<(Vec<u8>, Vec<u8>)>,
    /// Cached sort-and-combine result, invalidated on every mutation.
    /// Iterators (live per §3.7.10.2) re-read this between yields, so
    /// the cache absorbs the per-`next()` cost when there's no
    /// mutation. None on construction; populated lazily.
    sorted_cache: Option<Vec<(Vec<u8>, Vec<u8>)>>,
    /// Fetch §2.2 "guard". Default `None` (no restriction). See
    /// HeadersGuard.
    guard: HeadersGuard,
}

// ---------------------------------------------------------------------------
// Byte-rule helpers
// ---------------------------------------------------------------------------

/// HTTP token byte (RFC 9110 §5.6.2 `tchar`):
///   "!" / "#" / "$" / "%" / "&" / "'" / "*" / "+" / "-" / "." /
///   "^" / "_" / "`" / "|" / "~" / DIGIT / ALPHA
fn is_tchar(b: u8) -> bool {
    matches!(
        b,
        b'!' | b'#' | b'$' | b'%' | b'&' | b'\'' | b'*' | b'+'
            | b'-' | b'.' | b'^' | b'_' | b'`' | b'|' | b'~'
            | b'0'..=b'9' | b'A'..=b'Z' | b'a'..=b'z'
    )
}

/// Header name (Fetch §2.2): non-empty 1*tchar.
fn is_header_name(b: &[u8]) -> bool {
    !b.is_empty() && b.iter().all(|c| is_tchar(*c))
}

/// Header value (Fetch §2.2 "header value"):
///   - No leading or trailing 0x09 / 0x20 (TAB, SP).
///   - No 0x00 (NUL), 0x0A (LF), 0x0D (CR) anywhere.
/// Other bytes — including 0x80-0xFF, 0x0B, 0x0C, 0x7F, 0x01-0x08,
/// 0x0E-0x1F — are valid.
fn is_header_value(b: &[u8]) -> bool {
    !b.first().is_some_and(|c| *c == b' ' || *c == b'\t')
        && !b.last().is_some_and(|c| *c == b' ' || *c == b'\t')
        && !b.iter().any(|c| matches!(*c, 0x00 | 0x0A | 0x0D))
}

/// HTTP whitespace bytes (https://fetch.spec.whatwg.org/#http-whitespace-byte):
///   0x09 (TAB), 0x0A (LF), 0x0D (CR), 0x20 (SP).
/// Note 0x0A/0x0D ARE stripped at the ends here even though they are
/// forbidden in the middle. After normalize, an embedded LF/CR still
/// fails validate.
fn normalize_value(v: &[u8]) -> &[u8] {
    let is_ws = |b: u8| matches!(b, 0x09 | 0x0A | 0x0D | 0x20);
    let start = v.iter().position(|&b| !is_ws(b)).unwrap_or(v.len());
    let end = v.iter().rposition(|&b| !is_ws(b)).map_or(start, |i| i + 1);
    &v[start..end]
}

fn ascii_eq_ignore_case(a: &[u8], b: &[u8]) -> bool {
    a.len() == b.len()
        && a.iter()
            .zip(b.iter())
            .all(|(x, y)| x.to_ascii_lowercase() == y.to_ascii_lowercase())
}

// ---------------------------------------------------------------------------
// Headers algorithms (private)
// ---------------------------------------------------------------------------

impl Headers {
    /// Fetch §2.2 "validate":
    ///   1. If name is not a header name OR value is not a header value:
    ///      throw TypeError.
    ///   2. If headers' guard is "immutable": throw TypeError.
    ///   3. If headers' guard is "request" and (name, value) is a
    ///      forbidden request-header: return false.
    ///   4. If headers' guard is "response" and name is a forbidden
    ///      response-header name: return false.
    ///   5. Return true.
    ///
    /// v1 enforces step 1 (always) and step 2 (immutable guard, used
    /// for `Response.error()`). Steps 3-4 (request/response guards)
    /// are deferred per design D-17 — they only filter, never throw,
    /// so future guard expansion stays back-compatible.
    fn validate(&self, name: &[u8], value: &[u8]) -> Result<bool, OpError> {
        if !is_header_name(name) || !is_header_value(value) {
            return Err(OpError::type_error("Invalid header name or value"));
        }
        if matches!(self.guard, HeadersGuard::Immutable) {
            return Err(OpError::type_error(
                "Cannot mutate Headers with immutable guard",
            ));
        }
        Ok(true)
    }

    /// Like `validate` but for delete (which validates name only). Per
    /// Fetch §2.2.1 `dom-headers-delete` step 1, delete validates
    /// (name, "") — that empty value is a valid header value, so this
    /// is just a name-name check + the guard check.
    fn validate_for_delete(&self, name: &[u8]) -> Result<bool, OpError> {
        self.validate(name, b"")
    }

    /// Name-only well-formedness check used by `get` / `has`. Per
    /// Fetch §2.2.1 these are queries — they do NOT check the
    /// immutable guard (the spec validate step is only used by
    /// mutators).
    fn validate_query_name(&self, name: &[u8]) -> Result<(), OpError> {
        if !is_header_name(name) {
            return Err(OpError::type_error("Invalid header name"));
        }
        Ok(())
    }

    /// Set the headers' guard. Used by Response.error() and other
    /// callers that need to seal headers post-construction.
    pub fn set_guard(&mut self, g: HeadersGuard) {
        self.guard = g;
    }

    /// Append a header bypassing validation/guard checks. Used by
    /// internal callers (response builders) that need to populate
    /// headers from network data even on guarded instances. The data
    /// must already be validated (e.g., parsed from cyper response).
    #[allow(dead_code)]
    pub fn list_append_unchecked(&mut self, name: Vec<u8>, value: Vec<u8>) {
        self.list_append(name, value);
        self.invalidate_sort_cache();
    }

    /// Return the raw header list as a slice. Used by perf-critical
    /// internal callers that need to iterate headers without going
    /// through the JS-visible iterator protocol (which materializes
    /// a sorted+combined view).
    pub fn list(&self) -> &[(Vec<u8>, Vec<u8>)] {
        &self.list
    }

    /// "to append a header" §2.2.1: if list contains a header byte-case-
    /// insensitively matching name, set name to the first such match's
    /// name (preserving casing); push (name, value).
    fn list_append(&mut self, name: Vec<u8>, value: Vec<u8>) {
        let canonical = self
            .list
            .iter()
            .find(|(n, _)| ascii_eq_ignore_case(n, &name))
            .map(|(n, _)| n.clone())
            .unwrap_or(name);
        self.list.push((canonical, value));
    }

    /// Fetch §2.2.4 "header list set":
    ///   If list contains name, set the value of the first such header
    ///   to value and remove the others. Otherwise append (name, value).
    fn list_set(&mut self, name: Vec<u8>, value: Vec<u8>) {
        let mut found = false;
        let mut taken_value = Some(value);
        self.list.retain_mut(|(n, v)| {
            if ascii_eq_ignore_case(n, &name) {
                if !found {
                    found = true;
                    if let Some(new_v) = taken_value.take() {
                        *v = new_v;
                    }
                    true
                } else {
                    false
                }
            } else {
                true
            }
        });
        if !found {
            // taken_value is still Some when nothing matched.
            self.list.push((name, taken_value.unwrap_or_default()));
        }
    }

    /// Fetch §2.2.4 "header list delete": remove all entries with name
    /// (byte-case-insensitive).
    fn list_delete(&mut self, name: &[u8]) {
        self.list.retain(|(n, _)| !ascii_eq_ignore_case(n, name));
    }

    /// Fetch §2.2.4 "header list get":
    ///   1. If list does not contain name, return null.
    ///   2. Return the combined value given name and list.
    /// Combined value: all matching values joined by 0x2C 0x20 (", ")
    /// in list order. Note this applies to set-cookie too — getSetCookie
    /// is the un-joined accessor.
    fn list_get(&self, name: &[u8]) -> Option<Vec<u8>> {
        let mut buf: Option<Vec<u8>> = None;
        for (n, v) in &self.list {
            if ascii_eq_ignore_case(n, name) {
                match &mut buf {
                    None => buf = Some(v.clone()),
                    Some(out) => {
                        out.extend_from_slice(b", ");
                        out.extend_from_slice(v);
                    }
                }
            }
        }
        buf
    }

    fn invalidate_sort_cache(&mut self) {
        self.sorted_cache = None;
    }

    /// Fetch §2.2.4 "to sort and combine":
    ///   1. Let headers be a list of (name, value) pairs.
    ///   2. Let names be the result of converting each name in this to
    ///      lowercase, removing duplicates, and sorting bytewise.
    ///   3. For each name of names:
    ///      - If name is `set-cookie`: for each (n, v) in this where
    ///        lowercase(n) == name, append (name, v) to headers (in
    ///        list order, NOT lex order, within the cluster).
    ///      - Else: append (name, list_get(name)) — the joined value.
    fn sort_and_combine(&self) -> Vec<(Vec<u8>, Vec<u8>)> {
        let mut names: Vec<Vec<u8>> = self.list.iter().map(|(n, _)| n.to_ascii_lowercase()).collect();
        names.sort();
        names.dedup();

        let mut out = Vec::with_capacity(self.list.len());
        for name in names {
            if name.as_slice() == b"set-cookie" {
                for (n, v) in &self.list {
                    if ascii_eq_ignore_case(n, b"set-cookie") {
                        out.push((name.clone(), v.clone()));
                    }
                }
            } else if let Some(v) = self.list_get(&name) {
                out.push((name, v));
            }
        }
        out
    }

    /// Get (or compute and cache) the sort-and-combine result. The
    /// returned slice is the iterator's source per WebIDL §3.7.10.2's
    /// "value pairs to iterate over" hook. Mutations between
    /// `next()` calls invalidate the cache; the next call repopulates.
    fn value_pairs_to_iterate_over(&mut self) -> &[(Vec<u8>, Vec<u8>)] {
        if self.sorted_cache.is_none() {
            self.sorted_cache = Some(self.sort_and_combine());
        }
        self.sorted_cache.as_deref().unwrap()
    }
}

/// Read the underlying `Headers` Rust state from a JS `Headers`
/// wrapper object. Returns `None` if `obj` isn't a Headers wrapper
/// (no External in internal field 0, or null pointer).
///
/// The returned reference borrows the Box<Headers> in the wrapper's
/// internal field 0. The wrapper is single-threaded (per V8 isolate)
/// and the Box is dropped only by the V8 weak finalizer, which fires
/// after all JS callbacks complete — so a borrow that ends before
/// the next V8 entry is safe.
pub fn try_native_headers<'a>(
    scope: &mut v8::PinScope,
    obj: v8::Local<v8::Object>,
) -> Option<&'a Headers> {
    let ext = obj
        .get_internal_field(scope, 0)
        .and_then(|v| v8::Local::<v8::External>::try_from(v).ok())?;
    let ptr = ext.value() as *const Headers;
    if ptr.is_null() {
        return None;
    }
    Some(unsafe { &*ptr })
}

// ---------------------------------------------------------------------------
// Constructor + fill helpers
// ---------------------------------------------------------------------------

/// ECMA-262 7.3.11 GetMethod(V, P):
///   1. Let func be ? GetV(V, P).
///   2. If func is either undefined or null, return undefined.
///   3. If IsCallable(func) is false, throw a TypeError exception.
///   4. Return func.
fn get_method<'s>(
    scope: &mut v8::PinScope<'s, '_>,
    receiver: v8::Local<v8::Value>,
    key: v8::Local<v8::Value>,
) -> Result<Option<v8::Local<'s, v8::Function>>, OpError> {
    let obj: v8::Local<v8::Object> = receiver
        .try_into()
        .map_err(|_| OpError::type_error("Cannot get method of non-object"))?;
    let func = obj
        .get(scope, key)
        .ok_or_else(|| OpError::type_error("Property access threw"))?;
    if func.is_null_or_undefined() {
        return Ok(None);
    }
    if !func.is_function() {
        return Err(OpError::type_error("@@iterator is not callable"));
    }
    let f: v8::Local<v8::Function> = func
        .try_into()
        .map_err(|_| OpError::type_error("@@iterator is not callable"))?;
    Ok(Some(f))
}

fn is_object_like(v: v8::Local<v8::Value>) -> bool {
    v.is_object() || v.is_function()
}

/// Iterator-protocol drive: invoke `iter_fn(this=obj)`, then loop on
/// `result.next()` reading `done` and `value`. For each yielded value,
/// require it to be a 2-element sequence (name, value) of ByteStrings.
fn fill_from_iterable(
    scope: &mut v8::PinScope,
    obj: v8::Local<v8::Object>,
    iter_fn: v8::Local<v8::Function>,
    headers: &mut Headers,
) -> Result<(), OpError> {
    let iter_v = iter_fn
        .call(scope, obj.into(), &[])
        .ok_or_else(|| OpError::type_error("Iterator method threw"))?;
    let iter: v8::Local<v8::Object> = iter_v
        .try_into()
        .map_err(|_| OpError::type_error("Iterator did not return an object"))?;

    let next_key = v8::String::new(scope, "next").unwrap();
    let next_v = iter
        .get(scope, next_key.into())
        .ok_or_else(|| OpError::type_error("iter.next access threw"))?;
    let next_fn: v8::Local<v8::Function> = next_v
        .try_into()
        .map_err(|_| OpError::type_error("iter.next is not a function"))?;

    let done_key = v8::String::new(scope, "done").unwrap();
    let value_key = v8::String::new(scope, "value").unwrap();

    loop {
        let step_v = next_fn
            .call(scope, iter.into(), &[])
            .ok_or_else(|| OpError::type_error("iter.next() threw"))?;
        let step: v8::Local<v8::Object> = step_v
            .try_into()
            .map_err(|_| OpError::type_error("iter.next() did not return an object"))?;
        let done_v = step
            .get(scope, done_key.into())
            .ok_or_else(|| OpError::type_error("step.done access threw"))?;
        if done_v.boolean_value(scope) {
            break;
        }
        let value = step
            .get(scope, value_key.into())
            .ok_or_else(|| OpError::type_error("step.value access threw"))?;

        // Each yielded value must itself be a 2-element sequence.
        // WebIDL §3.2.18: if the inner value is not iterable, throw.
        if !is_object_like(value) {
            return Err(OpError::type_error(
                "Each header pair must be an iterable",
            ));
        }
        let pair_obj: v8::Local<v8::Object> = value
            .try_into()
            .map_err(|_| OpError::type_error("Each header pair must be an object"))?;

        // Read inner pair via Symbol.iterator. Per the spec, we use the
        // iterable protocol and require exactly 2 results.
        let sym_iter = v8::Symbol::get_iterator(scope);
        let inner_iter_fn = get_method(scope, pair_obj.into(), sym_iter.into())?
            .ok_or_else(|| OpError::type_error("Header pair is not iterable"))?;
        let inner_iter_v = inner_iter_fn
            .call(scope, pair_obj.into(), &[])
            .ok_or_else(|| OpError::type_error("Pair iterator threw"))?;
        let inner_iter: v8::Local<v8::Object> = inner_iter_v
            .try_into()
            .map_err(|_| OpError::type_error("Pair iterator did not return object"))?;
        let inner_next_v = inner_iter
            .get(scope, next_key.into())
            .ok_or_else(|| OpError::type_error("Pair iter.next access threw"))?;
        let inner_next_fn: v8::Local<v8::Function> = inner_next_v
            .try_into()
            .map_err(|_| OpError::type_error("Pair iter.next is not a function"))?;

        let mut elements: Vec<v8::Local<v8::Value>> = Vec::with_capacity(3);
        loop {
            let s = inner_next_fn
                .call(scope, inner_iter.into(), &[])
                .ok_or_else(|| OpError::type_error("Pair iter.next() threw"))?;
            let s_obj: v8::Local<v8::Object> = s
                .try_into()
                .map_err(|_| OpError::type_error("Pair iter.next() did not return an object"))?;
            let d = s_obj
                .get(scope, done_key.into())
                .ok_or_else(|| OpError::type_error("Pair step.done access threw"))?;
            if d.boolean_value(scope) {
                break;
            }
            let v = s_obj
                .get(scope, value_key.into())
                .ok_or_else(|| OpError::type_error("Pair step.value access threw"))?;
            elements.push(v);
            if elements.len() > 2 {
                // Don't read past the 3rd; we're going to throw anyway.
                break;
            }
        }

        if elements.len() != 2 {
            return Err(OpError::type_error(
                "Each header pair must have exactly two elements",
            ));
        }

        // ByteString conversion of each. May throw.
        let name_bytes = read_byte_string(scope, elements[0])?;
        let value_bytes = read_byte_string(scope, elements[1])?;

        // Use the IDL append (normalize+validate+list-append).
        headers.append(
            ByteString::from_bytes(name_bytes),
            ByteString::from_bytes(value_bytes),
        )?;
    }
    Ok(())
}

/// Record-of-ByteString-to-ByteString fill (Fetch §2.2.1
/// `dom-Headers` step 3, WebIDL `record<…>`):
///
///   1. Let keys be ? O.[[OwnPropertyKeys]] — ALL own keys including Symbols.
///   2. For each key in keys (in insertion order):
///      a. Let desc be ? O.[[GetOwnProperty]](key).
///      b. If desc is not undefined and desc.[[Enumerable]] is true:
///         i. Let typedKey be key converted to ByteString. Symbol → throws.
///         ii. Let value = ? Get(O, key); convert to ByteString.
///         iii. Append (typedKey, typedValue) to result.
fn fill_from_record(
    scope: &mut v8::PinScope,
    obj: v8::Local<v8::Object>,
    headers: &mut Headers,
) -> Result<(), OpError> {
    // Use ALL_PROPERTIES + KeepNumbers so Symbols and indices come
    // through without being stringified prematurely.
    let args = v8::GetPropertyNamesArgs {
        mode: v8::KeyCollectionMode::OwnOnly,
        property_filter: v8::PropertyFilter::ALL_PROPERTIES,
        index_filter: v8::IndexFilter::IncludeIndices,
        key_conversion: v8::KeyConversionMode::KeepNumbers,
    };
    let keys = obj
        .get_own_property_names(scope, args)
        .ok_or_else(|| OpError::type_error("Failed to enumerate own keys"))?;

    let enumerable_key = v8::String::new(scope, "enumerable").unwrap();

    for i in 0..keys.length() {
        let key_v = keys
            .get_index(scope, i)
            .ok_or_else(|| OpError::type_error("Key enumeration broke mid-iteration"))?;
        let key_name: v8::Local<v8::Name> = key_v
            .try_into()
            .map_err(|_| OpError::type_error("Property key is not a Name"))?;
        let desc = obj
            .get_own_property_descriptor(scope, key_name)
            .ok_or_else(|| OpError::type_error("Failed to get descriptor"))?;
        if desc.is_undefined() {
            continue;
        }
        let desc_obj: v8::Local<v8::Object> = desc
            .try_into()
            .map_err(|_| OpError::type_error("Descriptor is not an object"))?;
        let enumerable_v = desc_obj
            .get(scope, enumerable_key.into())
            .ok_or_else(|| OpError::type_error("Descriptor.enumerable access threw"))?;
        if !enumerable_v.boolean_value(scope) {
            continue;
        }

        // Symbol keys take this branch — read_byte_string will throw
        // TypeError when ToString hits the Symbol.
        let name_bytes = read_byte_string(scope, key_v)?;
        let value_v = obj
            .get(scope, key_v)
            .ok_or_else(|| OpError::type_error("Property get threw"))?;
        let value_bytes = read_byte_string(scope, value_v)?;
        headers.append(
            ByteString::from_bytes(name_bytes),
            ByteString::from_bytes(value_bytes),
        )?;
    }
    Ok(())
}

// ---------------------------------------------------------------------------
// Headers IDL surface
// ---------------------------------------------------------------------------

#[v8_class]
impl Headers {
    /// `new Headers(init?: HeadersInit)` per Fetch §2.2.1 `dom-Headers`.
    ///
    /// HeadersInit = sequence<sequence<ByteString>> | record<ByteString,
    /// ByteString>. Dispatch via WebIDL union (§3.2.20):
    ///   - undefined / null → empty Headers.
    ///   - object with @@iterator → sequence path.
    ///   - object without @@iterator → record path.
    ///   - anything else (number, primitive null is "the value null"
    ///     not an object) → TypeError.
    #[v8_constructor]
    fn new(
        scope: &mut v8::PinScope,
        init: v8::Local<v8::Value>,
    ) -> Result<Self, OpError> {
        let mut headers = Headers::default();
        if init.is_undefined() {
            return Ok(headers);
        }
        if init.is_null() {
            return Err(OpError::type_error(
                "Headers init may not be null",
            ));
        }
        if !is_object_like(init) {
            return Err(OpError::type_error(
                "Headers init must be an Object",
            ));
        }
        let obj: v8::Local<v8::Object> = init
            .try_into()
            .map_err(|_| OpError::type_error("Headers init must be an Object"))?;

        // Sequence-vs-record dispatch via GetMethod(@@iterator).
        let sym_iter = v8::Symbol::get_iterator(scope);
        match get_method(scope, obj.into(), sym_iter.into())? {
            Some(iter_fn) => fill_from_iterable(scope, obj, iter_fn, &mut headers)?,
            None => fill_from_record(scope, obj, &mut headers)?,
        }
        Ok(headers)
    }

    /// `append(name: ByteString, value: ByteString)` — Fetch §2.2.1
    /// `dom-headers-append`.
    ///   1. Normalize value.
    ///   2. Validate(name, normalized value). May throw or return false.
    ///   3. List-append.
    #[v8_method]
    fn append(&mut self, name: ByteString, value: ByteString) -> Result<(), OpError> {
        let value = normalize_value(value.as_slice()).to_vec();
        if !self.validate(name.as_slice(), &value)? {
            return Ok(());
        }
        self.list_append(name.into_bytes(), value);
        self.invalidate_sort_cache();
        Ok(())
    }

    /// `set(name: ByteString, value: ByteString)` — Fetch §2.2.1
    /// `dom-headers-set`. Same shape as append, but list-set.
    #[v8_method]
    fn set(&mut self, name: ByteString, value: ByteString) -> Result<(), OpError> {
        let value = normalize_value(value.as_slice()).to_vec();
        if !self.validate(name.as_slice(), &value)? {
            return Ok(());
        }
        self.list_set(name.into_bytes(), value);
        self.invalidate_sort_cache();
        Ok(())
    }

    /// `delete(name: ByteString)` — Fetch §2.2.1 `dom-headers-delete`.
    /// Step 1: validate(name, "") — throws on bad name AND on
    /// immutable guard.
    /// Renamed at JS surface from `delete_` to `delete` because the
    /// latter is a Rust keyword.
    #[v8_method]
    #[v8_name = "delete"]
    fn delete_(&mut self, name: ByteString) -> Result<(), OpError> {
        if !self.validate_for_delete(name.as_slice())? {
            return Ok(());
        }
        self.list_delete(name.as_slice());
        self.invalidate_sort_cache();
        Ok(())
    }

    /// `get(name: ByteString) -> ByteString?` — Fetch §2.2.1
    /// `dom-headers-get`. Validate name, then return the joined value
    /// (set-cookie joins too — un-joined is via `getSetCookie`). Read
    /// path: does NOT check the immutable guard.
    #[v8_method]
    fn get(&self, name: ByteString) -> Result<Option<Vec<u8>>, OpError> {
        self.validate_query_name(name.as_slice())?;
        Ok(self.list_get(name.as_slice()))
    }

    /// `has(name: ByteString) -> boolean` — Fetch §2.2.1
    /// `dom-headers-has`. Validate name then byte-case-insensitive
    /// existence check. Read path: does NOT check the immutable guard.
    ///
    /// `fastcall` opt-in: the macro emits a CFunction shim alongside
    /// the FunctionCallback so V8 TurboFan can inline the typed call
    /// at hot sites. ByteString → V8 SeqOneByteString fast-API type
    /// (the macro adapts via FastApiOneByteString::as_bytes + Vec
    /// copy). The Err arm — bad header name (`!is_header_name`) — is
    /// re-routed through CallbackScope::new(options) + throw_exception,
    /// which deopts and re-routes to the slow path next iteration.
    /// Promoted to Tier 1 by the 2026-05-04 httpGet regression bisect
    /// (`docs/perf/httpget-regression-2026-05-04.md`): scenarios.js
    /// does `request.headers.get("upgrade")` per request, but
    /// `headers.has` is a hot enough close-relative on the fetch
    /// dispatch path that fastcalling it delivers measurable savings
    /// per `crates/runtime-macros/TODO.md` ROI table.
    #[v8_method(fastcall)]
    fn has(&self, name: ByteString) -> Result<bool, OpError> {
        self.validate_query_name(name.as_slice())?;
        Ok(self
            .list
            .iter()
            .any(|(n, _)| ascii_eq_ignore_case(n, name.as_slice())))
    }

    /// `getSetCookie() -> sequence<ByteString>` — Fetch §2.2.1
    /// `dom-headers-getsetcookie`. Returns the un-joined set-cookie
    /// values in insertion order.
    #[v8_method]
    #[v8_name = "getSetCookie"]
    fn get_set_cookie(&self) -> Vec<Vec<u8>> {
        self.list
            .iter()
            .filter(|(n, _)| ascii_eq_ignore_case(n, b"set-cookie"))
            .map(|(_, v)| v.clone())
            .collect()
    }

    // forEach / keys() / values() / entries() / [@@iterator] aren't
    // routed through the macro because they need direct access to
    // `args.this()` (forEach: pass the Headers object as the third
    // callback arg per WebIDL §3.7.10.3; iterator factories: wire
    // `this` as the iterator's parent receiver). They are installed
    // directly in `install_global` via raw FunctionTemplate callbacks.
}

// ---------------------------------------------------------------------------
// HeadersIterator (live)
// ---------------------------------------------------------------------------

/// WebIDL §3.7.10 iteration kinds. Names match the spec.
#[derive(Debug, Clone, Copy)]
pub enum IterKind {
    /// "key" — `Headers.keys()` and the default `[Symbol.iterator]`'s key arg.
    Key,
    /// "value" — `Headers.values()`.
    Value,
    /// "key+value" — `Headers.entries()` and `[Symbol.iterator]`.
    KeyAndValue,
}

/// Iterator state. The `parent` Global keeps the source Headers wrapper
/// alive so its boxed Self stays valid; `index` advances per `next()`;
/// `kind` controls what each yielded value looks like.
pub struct HeadersIterator {
    parent: Option<v8::Global<v8::Object>>,
    index: usize,
    kind: IterKind,
}

impl Default for HeadersIterator {
    fn default() -> Self {
        HeadersIterator {
            parent: None,
            index: 0,
            kind: IterKind::KeyAndValue,
        }
    }
}

#[v8_class]
#[v8_to_string_tag = "Headers Iterator"]
#[v8_inherit_intrinsic = "IteratorPrototype"]
impl HeadersIterator {
    #[v8_method]
    fn next<'s>(
        &mut self,
        scope: &mut v8::PinScope<'s, '_>,
    ) -> v8::Local<'s, v8::Value> {
        let parent_global = match &self.parent {
            Some(g) => g,
            None => return iter_result_done(scope),
        };
        let parent = v8::Local::new(scope, parent_global);
        // Reach into the parent's internal field 0 and reborrow its
        // boxed Headers. Safe because:
        //   - V8 isolates are per-thread (AGENTS.md key invariant).
        //   - The Global keeps `parent` alive, which keeps the Box<Headers>
        //     alive (its finalizer runs only after GC).
        //   - The borrow scope is the entirety of next() — we don't
        //     yield across it.
        let ext = match parent
            .get_internal_field(scope, 0)
            .and_then(|v| v8::Local::<v8::External>::try_from(v).ok())
        {
            Some(e) => e,
            None => return iter_result_done(scope),
        };
        let headers: &mut Headers = unsafe { &mut *(ext.value() as *mut Headers) };

        let (n, v) = {
            let pairs = headers.value_pairs_to_iterate_over();
            if self.index >= pairs.len() {
                return iter_result_done(scope);
            }
            let (n, v) = &pairs[self.index];
            (n.clone(), v.clone())
        };
        self.index += 1;

        let value: v8::Local<v8::Value> = match self.kind {
            IterKind::KeyAndValue => {
                // Per WebIDL §3.7.10.3 "iteration result" for key+value:
                //   ArrayCreate(2) + CreateDataPropertyOrThrow.
                // Implemented in V8 as v8::Array::new + set_index — NOT
                // Array.of (which is observable via Array constructor).
                let arr = v8::Array::new(scope, 2);
                let n_str = v8::String::new_from_one_byte(
                    scope,
                    &n,
                    v8::NewStringType::Normal,
                )
                .unwrap();
                let v_str = v8::String::new_from_one_byte(
                    scope,
                    &v,
                    v8::NewStringType::Normal,
                )
                .unwrap();
                arr.set_index(scope, 0, n_str.into());
                arr.set_index(scope, 1, v_str.into());
                arr.into()
            }
            IterKind::Key => v8::String::new_from_one_byte(
                scope,
                &n,
                v8::NewStringType::Normal,
            )
            .unwrap()
            .into(),
            IterKind::Value => v8::String::new_from_one_byte(
                scope,
                &v,
                v8::NewStringType::Normal,
            )
            .unwrap()
            .into(),
        };
        iter_result(scope, value, false)
    }
}

/// Build `{ value, done }` per ECMA-262 7.4.7 "CreateIterResultObject".
fn iter_result<'s>(
    scope: &mut v8::PinScope<'s, '_>,
    value: v8::Local<v8::Value>,
    done: bool,
) -> v8::Local<'s, v8::Value> {
    let result = v8::Object::new(scope);
    let value_key = v8::String::new(scope, "value").unwrap();
    let done_key = v8::String::new(scope, "done").unwrap();
    let done_v = v8::Boolean::new(scope, done);
    result.set(scope, value_key.into(), value);
    result.set(scope, done_key.into(), done_v.into());
    result.into()
}

fn iter_result_done<'s>(scope: &mut v8::PinScope<'s, '_>) -> v8::Local<'s, v8::Value> {
    let undef = v8::undefined(scope);
    iter_result(scope, undef.into(), true)
}

// ---------------------------------------------------------------------------
// Iterator factory: keys/values/entries
// ---------------------------------------------------------------------------

/// Build a fresh HeadersIterator FunctionTemplate. We don't cache —
/// (a) Globals can't safely outlive their isolate (a thread-local
/// cache panics on the next isolate's reuse — V8 asserts the Handle
/// host matches), and (b) template builds are cheap relative to the
/// iteration cost. If profiling shows this as a hot path, switch to
/// an isolate-slot cache (v8::Isolate::set_slot).
fn iter_template<'s>(scope: &mut v8::PinScope<'s, '_>) -> v8::Local<'s, v8::FunctionTemplate> {
    HeadersIterator::install(scope)
}

// ---------------------------------------------------------------------------
// Custom install: wrap macro's install + add the iterator methods
// ---------------------------------------------------------------------------

/// Install `Headers` on the given global, wiring up the constructor,
/// methods, AND the parent-aware keys/values/entries iterator factory
/// methods that the macro can't emit (they need access to
/// `args.this()`).
///
/// This replaces `install_class`-style direct global writes that other
/// classes use. The ordering matters: we install via the macro's
/// `Headers::install` first, then patch the prototype's keys/values/
/// entries/[Symbol.iterator] methods to point at hand-rolled callbacks
/// that capture `this` per call.
/// Per-isolate cache of the Headers FunctionTemplate + prototype.
///
/// Set from `install_global`; consumed by the kernel-side fast-path
/// Request builder in `fetch_request::build_kernel_request` so it can
/// allocate a Headers wrapper without resolving `globalThis.Headers` and
/// without invoking the spec constructor (which walks the WebIDL
/// sequence/record dispatch + per-pair validation — overkill when the
/// kernel already has a clean header list).
pub struct HeadersTemplateSlot {
    pub class_tmpl: v8::Global<v8::FunctionTemplate>,
    pub prototype: v8::Global<v8::Object>,
}

pub fn install_global<'s>(
    scope: &mut v8::PinScope<'s, '_>,
    global: v8::Local<v8::Object>,
) {
    let tmpl = Headers::install(scope);
    let class_fn = tmpl.get_function(scope).unwrap();

    // Patch the prototype with parent-aware iterator factories. The
    // macro can't emit these because the user method body has no
    // direct access to `args.this()` (the iterator's parent receiver
    // — needed to wire as `parent: Global<Object>`).
    let proto_key = v8::String::new(scope, "prototype").unwrap();
    let proto_v = class_fn.get(scope, proto_key.into()).unwrap();
    let proto: v8::Local<v8::Object> = proto_v.try_into().unwrap();

    install_iter_factory(scope, proto, "keys", IterKind::Key);
    install_iter_factory(scope, proto, "values", IterKind::Value);
    install_iter_factory(scope, proto, "entries", IterKind::KeyAndValue);

    // forEach is hand-rolled too (the macro can't pass `this` as the
    // third callback arg per WebIDL §3.7.10.3 forEach algorithm).
    {
        let tmpl = v8::FunctionTemplate::new(scope, for_each_callback);
        let func = tmpl.get_function(scope).unwrap();
        let key = v8::String::new(scope, "forEach").unwrap();
        proto.set(scope, key.into(), func.into());
    }

    // [Symbol.iterator] aliases entries per WebIDL §3.7.10.
    let sym_iter = v8::Symbol::get_iterator(scope);
    let entries_key = v8::String::new(scope, "entries").unwrap();
    let entries_v = proto.get(scope, entries_key.into()).unwrap();
    proto.set(scope, sym_iter.into(), entries_v);

    let key = v8::String::new(scope, "Headers").unwrap();
    global.set(scope, key.into(), class_fn.into());

    // Stash the template + prototype for the kernel-side fast-path
    // Request builder. See `HeadersTemplateSlot`.
    let class_tmpl_g = v8::Global::new(scope, tmpl);
    let proto_g = v8::Global::new(scope, proto);
    scope.set_slot(HeadersTemplateSlot {
        class_tmpl: class_tmpl_g,
        prototype: proto_g,
    });
}

/// Build a Headers wrapper directly from a list of (name, value) byte
/// pairs that have already been validated upstream (e.g. by the HTTP
/// parser). Skips:
///   - the `globalThis.Headers` lookup,
///   - WebIDL sequence-vs-record dispatch in the constructor,
///   - per-pair `is_header_name` / `is_header_value` validation,
///   - normalize-then-validate algorithms in `append`.
///
/// The kernel uses this for the slow-path Request build — the upstream
/// HTTP layer already enforced header rules at parse time, so re-doing
/// them in V8 is wasted work. List ordering is preserved from the
/// caller; case is preserved per Fetch §2.2.1 (insertion order until
/// sort-and-combine fires on iteration).
///
/// Safety / correctness: the returned wrapper is indistinguishable from
/// one produced by `new Headers(record)` — internal field 0 holds a
/// Box<Headers> with the same shape, prototype is the user-visible
/// `Headers.prototype`. Mutator methods (set/append/delete) all work as
/// expected.
pub fn build_kernel_headers<'s>(
    scope: &mut v8::PinScope<'s, '_>,
    pairs: &[(String, String)],
) -> Option<v8::Local<'s, v8::Object>> {
    let (class_tmpl_g, proto_g) = {
        let slot = scope.get_slot::<HeadersTemplateSlot>()?;
        (slot.class_tmpl.clone(), slot.prototype.clone())
    };
    let class_tmpl = v8::Local::new(scope, class_tmpl_g);
    let inst_tmpl = class_tmpl.instance_template(scope);
    let obj = inst_tmpl.new_instance(scope)?;
    let proto = v8::Local::new(scope, proto_g);
    obj.set_prototype(scope, proto.into());

    // Build the Box<Headers> directly. The list field stores
    // (name_bytes, value_bytes) pairs in insertion order. We rely on
    // the upstream HTTP parser having already accepted these, so we
    // skip per-pair validation here.
    let mut headers = Headers::default();
    for (n, v) in pairs {
        // list_append_unchecked preserves insertion order + casing.
        // Fetch §2.2.1 `append` step 1's "reuse casing of first
        // byte-case-insensitively-matching name" still happens via
        // the underlying list_append; what we skip is the
        // normalize_value + per-byte validation on `name` and `value`
        // — already enforced upstream by the HTTP parser.
        headers.list_append_unchecked(n.as_bytes().to_vec(), v.as_bytes().to_vec());
    }

    install_headers_state(scope, obj, headers);
    Some(obj)
}

/// Variant of `build_kernel_headers` that takes ownership of the pair list.
///
/// Saves 2N byte-vec allocations per call vs the `&[(String, String)]`
/// variant — the (name, value) Strings are reused directly as the
/// internal `Vec<u8>` storage. Used by `fetch_response::build_kernel_response`
/// where the (name, value) list is freshly constructed from the HTTP
/// response and discarded after the wrapper is built.
pub fn build_kernel_headers_owned<'s>(
    scope: &mut v8::PinScope<'s, '_>,
    pairs: Vec<(String, String)>,
) -> Option<v8::Local<'s, v8::Object>> {
    let (class_tmpl_g, proto_g) = {
        let slot = scope.get_slot::<HeadersTemplateSlot>()?;
        (slot.class_tmpl.clone(), slot.prototype.clone())
    };
    let class_tmpl = v8::Local::new(scope, class_tmpl_g);
    let inst_tmpl = class_tmpl.instance_template(scope);
    let obj = inst_tmpl.new_instance(scope)?;
    let proto = v8::Local::new(scope, proto_g);
    obj.set_prototype(scope, proto.into());

    let mut headers = Headers::default();
    for (n, v) in pairs {
        // String -> Vec<u8> is a zero-copy buffer transfer (String wraps
        // a Vec<u8> internally; into_bytes consumes the String).
        headers.list_append_unchecked(n.into_bytes(), v.into_bytes());
    }

    install_headers_state(scope, obj, headers);
    Some(obj)
}

/// Common tail: box the `Headers` state, install it as internal field 0,
/// and register the GC finalizer that drops it.
fn install_headers_state(
    scope: &mut v8::PinScope,
    obj: v8::Local<v8::Object>,
    headers: Headers,
) {
    let boxed = Box::new(headers);
    let raw = Box::into_raw(boxed);
    let raw_addr = raw as usize;
    let ext = v8::External::new(scope, raw as *mut std::ffi::c_void);
    obj.set_internal_field(0, ext.into());

    let weak = v8::Weak::with_guaranteed_finalizer(
        scope,
        obj,
        Box::new(move || unsafe {
            drop(Box::from_raw(raw_addr as *mut Headers));
        }),
    );
    std::mem::forget(weak);
}

/// Hand-rolled `Headers.prototype.forEach(callback, thisArg?)` per
/// WebIDL §3.7.10.3 — the spec algorithm spells out "Set pairs to
/// idlObject's CURRENT list of value pairs to iterate over (it might
/// have changed)", so we re-read the live "value pairs" between
/// callback invocations. Mutation during forEach is observable.
///
/// Why hand-rolled (not via macro): the callback's third arg is the
/// Headers object itself — `args.this()` — and the macro doesn't
/// thread that through to user method bodies.
fn for_each_callback(
    scope: &mut v8::PinScope,
    args: v8::FunctionCallbackArguments,
    _rv: v8::ReturnValue,
) {
    let this_obj = args.this();
    let headers: &mut Headers = match this_obj
        .get_internal_field(scope, 0)
        .and_then(|v| v8::Local::<v8::External>::try_from(v).ok())
    {
        Some(e) => unsafe { &mut *(e.value() as *mut Headers) },
        None => {
            let msg = v8::String::new(scope, "Illegal invocation").unwrap();
            let exc = v8::Exception::type_error(scope, msg);
            scope.throw_exception(exc);
            return;
        }
    };

    let cb_arg = args.get(0);
    let cb_fn: v8::Local<v8::Function> = match cb_arg.try_into() {
        Ok(f) => f,
        Err(_) => {
            let msg = v8::String::new(scope, "forEach callback is not callable").unwrap();
            let exc = v8::Exception::type_error(scope, msg);
            scope.throw_exception(exc);
            return;
        }
    };
    let this_arg = args.get(1);

    let mut idx = 0usize;
    loop {
        let pair = {
            let pairs = headers.value_pairs_to_iterate_over();
            if idx >= pairs.len() {
                return;
            }
            pairs[idx].clone()
        };
        idx += 1;
        let (n, v) = pair;

        let value_v =
            v8::String::new_from_one_byte(scope, &v, v8::NewStringType::Normal).unwrap();
        let key_v =
            v8::String::new_from_one_byte(scope, &n, v8::NewStringType::Normal).unwrap();
        let cb_args = [value_v.into(), key_v.into(), this_obj.into()];

        // call() returns None if the callback threw — V8 has the
        // exception pending. Stop iteration; the throw propagates to
        // the JS caller of forEach.
        if cb_fn.call(scope, this_arg, &cb_args).is_none() {
            return;
        }
    }
}

fn install_iter_factory<'s>(
    scope: &mut v8::PinScope<'s, '_>,
    proto: v8::Local<v8::Object>,
    name: &str,
    kind: IterKind,
) {
    // Encode the kind as a small integer in the External so the same
    // raw callback can dispatch all three factory methods.
    let kind_marker: i64 = match kind {
        IterKind::Key => 0,
        IterKind::Value => 1,
        IterKind::KeyAndValue => 2,
    };
    let data = v8::Integer::new(scope, kind_marker as i32);
    let tmpl = v8::FunctionTemplate::builder(iter_factory_callback)
        .data(data.into())
        .build(scope);
    let func = tmpl.get_function(scope).unwrap();
    let key = v8::String::new(scope, name).unwrap();
    proto.set(scope, key.into(), func.into());
}

fn iter_factory_callback(
    scope: &mut v8::PinScope,
    args: v8::FunctionCallbackArguments,
    mut rv: v8::ReturnValue,
) {
    // `this` must be a Headers instance — its internal field 0 holds
    // the Box<Headers>. Verify by ext extraction; throw on mismatch.
    let this_obj = args.this();
    let _headers_ptr = match this_obj
        .get_internal_field(scope, 0)
        .and_then(|v| v8::Local::<v8::External>::try_from(v).ok())
    {
        Some(e) => e.value() as *mut Headers,
        None => {
            let msg = v8::String::new(scope, "Illegal invocation").unwrap();
            let exc = v8::Exception::type_error(scope, msg);
            scope.throw_exception(exc);
            return;
        }
    };

    // Decode kind from the FunctionTemplate data slot.
    let kind = {
        let raw = args.data();
        let n = if let Ok(int) = v8::Local::<v8::Integer>::try_from(raw) {
            int.value()
        } else {
            2
        };
        match n {
            0 => IterKind::Key,
            1 => IterKind::Value,
            _ => IterKind::KeyAndValue,
        }
    };

    // Build a HeadersIterator object via the cached template; set its
    // internal field to a fresh Box<HeadersIterator> with parent set.
    let it_tmpl = iter_template(scope);
    let inst_tmpl = it_tmpl.instance_template(scope);
    let it_obj = match inst_tmpl.new_instance(scope) {
        Some(o) => o,
        None => {
            let msg = v8::String::new(scope, "Failed to allocate iterator").unwrap();
            let exc = v8::Exception::error(scope, msg);
            scope.throw_exception(exc);
            return;
        }
    };
    // Wire prototype to %Iterator.prototype% chain via the template's
    // get_function once (so the prototype is stamped with `next` and
    // chained via #[v8_inherit_intrinsic]).
    let it_class_fn = it_tmpl.get_function(scope).unwrap();
    let proto_key = v8::String::new(scope, "prototype").unwrap();
    let it_proto_v = it_class_fn.get(scope, proto_key.into()).unwrap();
    it_obj.set_prototype(scope, it_proto_v);

    let parent_global = v8::Global::new(scope, this_obj);
    let boxed = Box::new(HeadersIterator {
        parent: Some(parent_global),
        index: 0,
        kind,
    });
    let raw = Box::into_raw(boxed);
    let raw_addr = raw as usize;
    let ext = v8::External::new(scope, raw as *mut std::ffi::c_void);
    it_obj.set_internal_field(0, ext.into());

    // Finalizer: same shape as the macro's gen_box_and_install_finalizer.
    let weak = v8::Weak::with_guaranteed_finalizer(
        scope,
        it_obj,
        Box::new(move || unsafe {
            drop(Box::from_raw(raw_addr as *mut HeadersIterator));
        }),
    );
    std::mem::forget(weak);

    rv.set(it_obj.into());
}

// ---------------------------------------------------------------------------
// External callers: project a Headers wrapper to its native struct
// ---------------------------------------------------------------------------

/// Set the guard on a Headers V8 wrapper. Caller must guarantee that
/// `headers_obj` is a Headers wrapper (typically because they just
/// minted it via the global constructor).
///
/// Used by `Response.error()` to seal its empty headers per Fetch
/// §6.2.4 step 4: "Set response's headers' guard to immutable."
pub fn seal_immutable(scope: &mut v8::PinScope, headers_obj: v8::Local<v8::Object>) {
    let ext_v = match headers_obj.get_internal_field(scope, 0) {
        Some(v) => v,
        None => return,
    };
    let ext: v8::Local<v8::External> = match ext_v.try_into() {
        Ok(e) => e,
        Err(_) => return,
    };
    let ptr = ext.value() as *mut Headers;
    if ptr.is_null() {
        return;
    }
    // SAFETY: the V8 wrapper owns the Box<Headers> via External; we
    // hold a transient `&mut` borrow only for the duration of this
    // call.
    unsafe { (*ptr).set_guard(HeadersGuard::Immutable) };
}
