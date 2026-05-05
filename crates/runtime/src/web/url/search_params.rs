//! Native `URLSearchParams` per WHATWG URL §6
//! (https://url.spec.whatwg.org/#urlsearchparams).
//!
//! Two operating modes:
//!
//! - **Standalone**: `entries` owns the (name, value) list outright.
//!   Constructed via `new URLSearchParams(...)` directly.
//! - **Bound to URL**: `parent_url` holds a `Global<v8::Object>` of the
//!   parent URL JS wrapper. Every read re-syncs `entries` from the
//!   parent's `ada_url::Url::search()`; every write serializes back via
//!   `set_search()`. This is the live two-way sync the spec mandates
//!   for `url.searchParams` (`[SameObject]` per §4.5).
//!
//! Iterator (entries / keys / values / [Symbol.iterator]) is
//! implemented separately as `URLSearchParamsIterator` because its
//! `next()` callback needs `args.this()` to reach the parent URL —
//! something the macro can't thread through user method bodies.
//!
//! Note: the macro only emits same-name getter+setter pairing as two
//! separate `set_accessor_property` calls (which V8 would reject), so
//! `size` is installed by hand in `install_global`. No setter is needed
//! for `size` (read-only per spec).

use crate::state::OpError;
use crate::url_native::helpers::{
    read_usv_string, url_encoded_parse, url_encoded_serialize, USVString,
};
use crate::url_native::url::URL;

#[allow(unused_imports)]
use zeroship_runtime_macros::{
    v8_class, v8_constructor, v8_inherit_intrinsic, v8_iterable, v8_method, v8_to_string_tag,
};

// ---------------------------------------------------------------------------
// URLSearchParams struct
// ---------------------------------------------------------------------------

/// A WHATWG URLSearchParams. Either standalone (owns its entry list) or
/// bound to a parent URL (re-reads/writes through ada-url's set_search).
#[derive(Default)]
pub struct URLSearchParams {
    /// Insertion-ordered (name, value) pairs.
    ///
    /// In bound mode this is repopulated from the parent URL on every
    /// read — see `sync_from_parent` / `flush_to_parent`.
    entries: Vec<(String, String)>,
    /// `Some(weak)` iff this URLSearchParams is bound to a URL via
    /// `url.searchParams`. `None` for standalone instances.
    ///
    /// Stored as a `Weak<Object>` (NOT a `Global<Object>`) to break the
    /// reference cycle URL ↔ SP. The URL holds a strong `Global` to its
    /// SP wrapper (forward direction; needed for `[SameObject]` cache);
    /// the SP holds a weak back-reference. When the URL is GC'd the
    /// weak fails to upgrade and the SP transparently falls back to
    /// standalone semantics (sync/flush become no-ops).
    parent_url: Option<v8::Weak<v8::Object>>,
    /// Cache of the parent's last-observed `inner.search()`. Skipping
    /// the re-parse when the search hasn't changed turns iterator
    /// for-of from O(n²) to O(n) (see C3).
    last_seen_search: String,
}

impl URLSearchParams {
    /// Build a `URLSearchParams` already bound to a URL JS wrapper. The
    /// parent's `internal_field(0)` must hold a `Box<URL>`.
    ///
    /// Takes a `Weak<Object>` rather than a `Global<Object>` to avoid
    /// the cycle leak documented on `parent_url`.
    pub fn bound_to(parent: v8::Weak<v8::Object>) -> Self {
        URLSearchParams {
            entries: Vec::new(),
            parent_url: Some(parent),
            last_seen_search: String::new(),
        }
    }

    /// Helper: in bound mode, re-read entries from the parent URL's
    /// search component before any access. Standalone (no parent, or
    /// parent already GC'd) returns `&self.entries` as-is.
    ///
    /// Caches `last_seen_search` so back-to-back reads (notably the
    /// iterator's `next()` per C3) skip the re-parse when the parent's
    /// search hasn't changed. The cache also covers the orphan case:
    /// once the weak fails to upgrade, `entries` keeps whatever it had
    /// at last sync — effectively detaching to standalone.
    fn sync_from_parent(&mut self, scope: &mut v8::PinScope) {
        let Some(parent_weak) = &self.parent_url else {
            return;
        };
        let Some(parent) = parent_weak.to_local(scope) else {
            // Parent URL has been GC'd. Behave as standalone: keep
            // current entries, no further sync.
            return;
        };
        // SAFETY: parent's internal field 0 was set by the URL
        // constructor (or URL::install_search_params_global); the Weak
        // upgrade keeps the wrapper alive for this scope, which keeps
        // the Box<URL> alive (its finalizer runs only after GC). No
        // concurrent &mut URL alias exists for the duration of this
        // local borrow — V8 is single-threaded per isolate.
        let url_ptr = match url_ptr_from_object(parent, scope) {
            Some(p) => p,
            None => return,
        };
        let url_inst: &mut URL = unsafe { &mut *url_ptr };
        let search = url_inst.inner.search();
        if search == self.last_seen_search {
            // Parent hasn't changed since last sync — skip re-parse.
            return;
        }
        // Strip the leading "?" if present (search() returns it
        // included; the urlencoded parser doesn't expect it).
        let stripped = search.strip_prefix('?').unwrap_or(search);
        self.entries = url_encoded_parse(stripped);
        self.last_seen_search = search.to_string();
    }

    /// Helper: in bound mode, serialize current entries and push back
    /// to the parent URL's search component via ada-url's
    /// `set_search`. Standalone (parent GC'd or never bound) is a
    /// no-op.
    fn flush_to_parent(&mut self, scope: &mut v8::PinScope) {
        let Some(parent_weak) = &self.parent_url else {
            return;
        };
        let Some(parent) = parent_weak.to_local(scope) else {
            // Parent URL has been GC'd. Mutations are still applied to
            // self.entries; just don't fail trying to write back.
            return;
        };
        let url_ptr = match url_ptr_from_object(parent, scope) {
            Some(p) => p,
            None => return,
        };
        // SAFETY: see sync_from_parent.
        let url_inst: &mut URL = unsafe { &mut *url_ptr };
        if self.entries.is_empty() {
            // Per §6.2 step 5: if the serialization is the empty
            // string, set the URL's query to null (clears it).
            url_inst.inner.set_search(None);
        } else {
            let serialized = url_encoded_serialize(&self.entries);
            // Spec: set_search receives the serialized form WITHOUT the
            // leading "?". ada-url's set_search accepts either; pass
            // raw (matches the spec call site).
            url_inst.inner.set_search(Some(&serialized));
        }
        // Cache what the parent now holds so the next sync_from_parent
        // can short-circuit (the urlencoded round-trip is lossy on `+`
        // vs `%20` so we read it back rather than caching the
        // serialized form we just wrote).
        self.last_seen_search = url_inst.inner.search().to_string();
    }
}

// Brand check is now provided by the `#[v8_class]` macro: the
// auto-generated `__brand_check_URLSearchParams` walks `obj`'s
// [[Prototype]] chain for the cached `URLSearchParams.prototype` and
// is invoked at the top of every method/getter/iterator-factory/
// forEach callback. The pre-MAC-09 hand-rolled `is_url_search_params`
// helper here became dead once the iterator factory + forEach moved
// into the macro emit (M4/M5 fixes are now expressed by the same
// brand-check on every entry point).

/// Reach into a V8 object's internal field 0 and recover the raw
/// pointer to the boxed `URL` if present. Returns `None` for non-URL
/// receivers.
///
/// SAFETY contract for callers (C2):
///   - The External pointer was set by `URL::install`'s constructor
///     callback (or by `URL::install_search_params_global`).
///   - The caller must hold a Local (or some other liveness anchor)
///     keeping the parent V8 wrapper alive for the duration of any
///     deref. The wrapper's strong reference keeps the Box<URL> alive
///     (its weak finalizer runs only after GC).
///   - No concurrent `&mut URL` may exist for the local borrow's
///     lifetime. V8 is single-threaded per isolate (AGENTS.md key
///     invariant), so the only risk is reentrance within this thread —
///     callers must not call back into JS while the borrow is live.
///
/// The previous API returned `&'a mut URL` with an unbounded `'a`
/// generic; that allowed callers to synthesize any lifetime, including
/// `'static`, completely independent of the actual liveness of `obj`.
/// Returning a raw pointer forces the unsafe `&mut *` at each use site
/// where the safety conditions can be reasoned about locally.
pub(crate) fn url_ptr_from_object(
    obj: v8::Local<v8::Object>,
    scope: &mut v8::PinScope,
) -> Option<*mut URL> {
    let ext = obj
        .get_internal_field(scope, 0)
        .and_then(|v| v8::Local::<v8::External>::try_from(v).ok())?;
    Some(ext.value() as *mut URL)
}

// ---------------------------------------------------------------------------
// URLSearchParams class — IDL surface per https://url.spec.whatwg.org/#urlsearchparams
// ---------------------------------------------------------------------------

#[v8_class]
#[v8_iterable(key = USVString, value = USVString, mode = live)]
impl URLSearchParams {
    /// `new URLSearchParams(init?)`: per §6.2 / IDL union of
    ///   - USVString (parsed as application/x-www-form-urlencoded)
    ///   - sequence<sequence<USVString>> (each inner = [name, value])
    ///   - record<USVString, USVString>
    ///
    /// Dispatch via WebIDL §3.2.20:
    ///   - undefined / null / empty → empty.
    ///   - object with @@iterator → sequence path.
    ///   - object without @@iterator → record path.
    ///   - else → USVString path.
    #[v8_constructor]
    fn new(
        scope: &mut v8::PinScope,
        init: v8::Local<v8::Value>,
    ) -> Result<Self, OpError> {
        let mut sp = URLSearchParams::default();
        if init.is_undefined() || init.is_null() {
            return Ok(sp);
        }

        // sequence-vs-record dispatch first: anything with @@iterator
        // (Array, Map, custom iterables) takes the sequence path; only
        // plain objects fall through to the record path. The string
        // path is the catch-all.
        //
        // C5: Per WebIDL §3.10 record conversion, ANY non-iterable
        // object goes through the record path. The previous code
        // excluded ArrayBuffer/ArrayBufferView from the object branch,
        // forcing them into the string path where ToString produced
        // "[object ArrayBuffer]" or similar — non-spec garbage. The
        // correct behaviour:
        //   - Plain ArrayBuffer (no @@iterator): record path → empty
        //     (no own enumerable string-keyed properties).
        //   - Typed-array views (Uint8Array etc.) ARE iterable via
        //     @@iterator (yielding numbers), so they take the sequence
        //     path and fail naturally on the per-pair length check.
        if init.is_object() {
            let obj: v8::Local<v8::Object> = match init.try_into() {
                Ok(o) => o,
                Err(_) => return Ok(sp),
            };
            let sym_iter = v8::Symbol::get_iterator(scope);
            let iter_method_v = obj
                .get(scope, sym_iter.into())
                .ok_or_else(|| OpError::type_error("@@iterator access threw"))?;
            if iter_method_v.is_function() {
                fill_from_iterable(scope, obj, iter_method_v.try_into().unwrap(), &mut sp.entries)?;
                return Ok(sp);
            }
            // No @@iterator — record path.
            fill_from_record(scope, obj, &mut sp.entries)?;
            return Ok(sp);
        }

        // String path. ToUSVString first, then strip leading "?", then
        // urlencoded parse.
        let s = read_usv_string(scope, init)
            .ok_or_else(|| OpError::type_error("Cannot convert init to USVString"))?;
        let stripped = s.strip_prefix('?').unwrap_or(&s);
        sp.entries = url_encoded_parse(stripped);
        Ok(sp)
    }

    /// `append(name, value)` per §6.2.1. Push `(name, value)` to the
    /// list. Both args are USVStrings.
    #[v8_method]
    fn append(
        &mut self,
        scope: &mut v8::PinScope,
        name: USVString,
        value: USVString,
    ) -> Result<(), OpError> {
        self.sync_from_parent(scope);
        self.entries.push((name.into_string(), value.into_string()));
        self.flush_to_parent(scope);
        Ok(())
    }

    /// `delete(name, value?)` per §6.2.2 (latest spec — accepts
    /// optional value):
    ///   - 1 arg: remove all (n, _) pairs where n == name.
    ///   - 2 args: remove all (n, v) pairs where n == name AND v == value.
    ///
    /// The 2-arg form is recent (URLSearchParams `value?` parameter
    /// added 2023). The polyfill ignored the 2nd arg silently.
    #[v8_method]
    fn delete(
        &mut self,
        scope: &mut v8::PinScope,
        name: USVString,
        value: Option<USVString>,
    ) -> Result<(), OpError> {
        let n = name.into_string();
        let v_opt = value.map(USVString::into_string);
        self.sync_from_parent(scope);
        self.entries.retain(|(en, ev)| match &v_opt {
            Some(target_v) => !(en == &n && ev == target_v),
            None => en != &n,
        });
        self.flush_to_parent(scope);
        Ok(())
    }

    /// `get(name)` per §6.2.3 — return the value of the first pair with
    /// matching name, or null.
    #[v8_method]
    fn get(
        &mut self,
        scope: &mut v8::PinScope,
        name: USVString,
    ) -> Result<Option<String>, OpError> {
        let n = name.into_string();
        self.sync_from_parent(scope);
        Ok(self
            .entries
            .iter()
            .find(|(en, _)| en == &n)
            .map(|(_, v)| v.clone()))
    }

    /// `getAll(name)` per §6.2.4 — return all values whose name matches.
    #[v8_method]
    #[v8_name = "getAll"]
    fn get_all(
        &mut self,
        scope: &mut v8::PinScope,
        name: USVString,
    ) -> Result<Vec<String>, OpError> {
        let n = name.into_string();
        self.sync_from_parent(scope);
        Ok(self
            .entries
            .iter()
            .filter(|(en, _)| en == &n)
            .map(|(_, v)| v.clone())
            .collect())
    }

    /// `has(name, value?)` per §6.2.5 (latest spec). 1-arg form is
    /// "any pair with matching name"; 2-arg is "any pair with matching
    /// name AND value".
    #[v8_method]
    fn has(
        &mut self,
        scope: &mut v8::PinScope,
        name: USVString,
        value: Option<USVString>,
    ) -> Result<bool, OpError> {
        let n = name.into_string();
        let v_opt = value.map(USVString::into_string);
        self.sync_from_parent(scope);
        Ok(self.entries.iter().any(|(en, ev)| match &v_opt {
            Some(target_v) => en == &n && ev == target_v,
            None => en == &n,
        }))
    }

    /// `set(name, value)` per §6.2.6 — if the list contains pairs with
    /// matching name, set the first such value to `value` and remove
    /// the others. Otherwise append `(name, value)`.
    #[v8_method]
    fn set(
        &mut self,
        scope: &mut v8::PinScope,
        name: USVString,
        value: USVString,
    ) -> Result<(), OpError> {
        let n = name.into_string();
        let v = value.into_string();
        self.sync_from_parent(scope);
        let mut found = false;
        let mut taken_v = Some(v);
        self.entries.retain_mut(|(en, ev)| {
            if en == &n {
                if !found {
                    found = true;
                    if let Some(new_v) = taken_v.take() {
                        *ev = new_v;
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
            // taken_v still Some when nothing matched.
            self.entries.push((n, taken_v.unwrap_or_default()));
        }
        self.flush_to_parent(scope);
        Ok(())
    }

    /// `sort()` per §6.2.7 — stable-sort the list by name in code-unit
    /// order (i.e. UTF-16 lex order of the name). The relative order
    /// of pairs with the same name is preserved.
    #[v8_method]
    fn sort(&mut self, scope: &mut v8::PinScope) {
        self.sync_from_parent(scope);
        // §6.2.7 step 1: stable sort by code-unit order. Rust strings
        // are UTF-8 so we sort by encoded UTF-16 to match the spec
        // letter — but for ASCII names + Latin-1 the byte order is
        // identical, and pure-ASCII is the common case. Use the more
        // explicit code-unit comparator.
        self.entries.sort_by(|a, b| code_unit_cmp(&a.0, &b.0));
        self.flush_to_parent(scope);
    }

    /// `toString()` per §6.2.8 — application/x-www-form-urlencoded
    /// serialization of the entries.
    #[v8_method]
    #[v8_name = "toString"]
    fn to_string(&mut self, scope: &mut v8::PinScope) -> String {
        self.sync_from_parent(scope);
        url_encoded_serialize(&self.entries)
    }

    /// `size` getter per §6.2.9 — number of (name, value) pairs.
    /// Read-only attribute; sync_from_parent ensures it reflects the
    /// parent URL's current search component when bound.
    #[v8_getter]
    fn size(&mut self, scope: &mut v8::PinScope) -> u32 {
        self.sync_from_parent(scope);
        self.entries.len() as u32
    }

    /// `value_pairs(&mut self, scope)` — the WebIDL §3.7.10.2 "value
    /// pairs to iterate over" hook for `#[v8_iterable(mode = live)]`.
    /// Re-syncs from the parent URL on every call (the macro invokes
    /// this once per `next()` and once per forEach iteration), then
    /// returns the entries as `(USVString, USVString)` pairs which the
    /// macro encodes back to V8 strings on each yield.
    fn value_pairs(
        &mut self,
        scope: &mut v8::PinScope,
    ) -> Vec<(USVString, USVString)> {
        self.sync_from_parent(scope);
        self.entries
            .iter()
            .map(|(n, v)| (USVString::from(n.clone()), USVString::from(v.clone())))
            .collect()
    }
}

/// Compare two strings by UTF-16 code units (lex order). Pure ASCII
/// strings collapse to byte order; non-ASCII names need the proper
/// UTF-16 traversal because supplementary code points are surrogate
/// pairs in UTF-16 but ≥ U+10000 in Rust `char`.
fn code_unit_cmp(a: &str, b: &str) -> std::cmp::Ordering {
    let mut ai = a.encode_utf16();
    let mut bi = b.encode_utf16();
    loop {
        match (ai.next(), bi.next()) {
            (Some(x), Some(y)) => match x.cmp(&y) {
                std::cmp::Ordering::Equal => continue,
                ord => return ord,
            },
            (Some(_), None) => return std::cmp::Ordering::Greater,
            (None, Some(_)) => return std::cmp::Ordering::Less,
            (None, None) => return std::cmp::Ordering::Equal,
        }
    }
}

// ---------------------------------------------------------------------------
// fill_from_iterable / fill_from_record
// ---------------------------------------------------------------------------

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
        return Err(OpError::type_error("Method is not callable"));
    }
    let f: v8::Local<v8::Function> = func
        .try_into()
        .map_err(|_| OpError::type_error("Method is not callable"))?;
    Ok(Some(f))
}

/// Iterate `obj`'s @@iterator, requiring each yielded value to be a
/// 2-element sequence (name, value) of USVStrings. Per §6.2 step 1
/// init = sequence<sequence<USVString>>.
fn fill_from_iterable(
    scope: &mut v8::PinScope,
    obj: v8::Local<v8::Object>,
    iter_fn: v8::Local<v8::Function>,
    out: &mut Vec<(String, String)>,
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

        // Each pair must itself be iterable yielding exactly 2 values.
        if !(value.is_object() || value.is_function()) {
            return Err(OpError::type_error(
                "Each URLSearchParams pair must be an iterable",
            ));
        }
        let pair_obj: v8::Local<v8::Object> = value
            .try_into()
            .map_err(|_| OpError::type_error("Each URLSearchParams pair must be an object"))?;

        let sym_iter = v8::Symbol::get_iterator(scope);
        let inner_iter_fn = get_method(scope, pair_obj.into(), sym_iter.into())?
            .ok_or_else(|| OpError::type_error("Pair is not iterable"))?;
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
                .map_err(|_| OpError::type_error("Pair iter.next() not an object"))?;
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
                break;
            }
        }

        if elements.len() != 2 {
            return Err(OpError::type_error(
                "Each URLSearchParams pair must have exactly two elements",
            ));
        }

        let name = read_usv_string(scope, elements[0])
            .ok_or_else(|| OpError::type_error("Cannot convert pair[0] to USVString"))?;
        let value = read_usv_string(scope, elements[1])
            .ok_or_else(|| OpError::type_error("Cannot convert pair[1] to USVString"))?;
        out.push((name, value));
    }
    Ok(())
}

/// Iterate `obj`'s own enumerable properties, mapping each to a
/// (name, value) pair of USVStrings. Per WebIDL `record<USVString,
/// USVString>` (§3.10 conversion):
///
///   - Result is an "ordered map" (Map in JS).
///   - For each key/value pair in init: `result[key] = value`.
///   - Insertion order is preserved by the FIRST insertion of a
///     given key; subsequent assignments update value in place.
///
/// USVString conversion replaces lone surrogates with U+FFFD —
/// which means two distinct JS keys (`"\uD835x"` and `"\uD83Dx"`)
/// can collide on `"\uFFFDx"`. Spec resolution: dedup by USV-key,
/// last-write-wins for value, FIRST-insertion-wins for position.
fn fill_from_record(
    scope: &mut v8::PinScope,
    obj: v8::Local<v8::Object>,
    out: &mut Vec<(String, String)>,
) -> Result<(), OpError> {
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

    // Order-preserving map: index of (key) in `out`. None means
    // not-yet-inserted.
    let mut key_index: std::collections::HashMap<String, usize> =
        std::collections::HashMap::new();

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
            .map_err(|_| OpError::type_error("Descriptor not an object"))?;
        let enumerable_v = desc_obj
            .get(scope, enumerable_key.into())
            .ok_or_else(|| OpError::type_error("Descriptor.enumerable access threw"))?;
        if !enumerable_v.boolean_value(scope) {
            continue;
        }

        let name = read_usv_string(scope, key_v)
            .ok_or_else(|| OpError::type_error("Cannot convert key to USVString"))?;
        let value_v = obj
            .get(scope, key_v)
            .ok_or_else(|| OpError::type_error("Property get threw"))?;
        let value = read_usv_string(scope, value_v)
            .ok_or_else(|| OpError::type_error("Cannot convert value to USVString"))?;

        if let Some(&idx) = key_index.get(&name) {
            // Existing key — update value in place.
            out[idx].1 = value;
        } else {
            key_index.insert(name.clone(), out.len());
            out.push((name, value));
        }
    }
    Ok(())
}

// ---------------------------------------------------------------------------
// install_global — install URLSearchParams; iterator surface comes from
// the macro's `#[v8_iterable(mode = live)]` emit.
// ---------------------------------------------------------------------------

pub fn install_global<'s>(
    scope: &mut v8::PinScope<'s, '_>,
    global: v8::Local<v8::Object>,
) -> v8::Local<'s, v8::Function> {
    let tmpl = URLSearchParams::install(scope);
    let class_fn = tmpl.get_function(scope).unwrap();

    let key = v8::String::new(scope, "URLSearchParams").unwrap();
    global.set(scope, key.into(), class_fn.into());
    class_fn
}
