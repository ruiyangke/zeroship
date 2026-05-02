//! Native `Blob` per WHATWG File API §3 — https://w3c.github.io/FileAPI/#blob-section.
//!
//! Replaces the JS Blob polyfill that lived in `embed/blob.js` (78 LOC).
//! The polyfill used POJO instances (no `instanceof` brand check, no
//! `@@toStringTag`), didn't validate `parts` per spec, and `Blob.stream()`
//! returned a JS-side fake instead of a native ReadableStream.
//!
//! ## Storage
//!
//! Bytes live behind an `Rc<Vec<u8>>` so `slice()` can share the
//! underlying buffer cheaply (a sliced Blob holds a different
//! `(start, len)` window over the same `Rc`). Two adjacent Blobs that
//! came from the same `slice()` chain share one allocation.
//!
//! ## Spec map (key sections)
//!
//! - Constructor: §3.2 (https://w3c.github.io/FileAPI/#constructorBlob)
//! - `size` / `type`: §3.1
//! - `slice`: §3.3.6
//! - `stream`: §3.3.7
//! - `text`: §3.3.8
//! - `arrayBuffer`: §3.3.9
//! - `bytes`: spec PR (newer, "blob bytes()" section in current ED)
//!
//! ## Deferred from v1
//!
//! - **Line ending normalization** for `endings: "native"`: §3.2 step 3
//!   says replace LF/CR/CRLF with the platform's native ending. We
//!   accept the option but treat it as a no-op (the spec also says
//!   "transparent" is the default and we never match a system that
//!   isn't \n; on Unix even the "native" path is identity).
//! - **Blob URL store**: `URL.createObjectURL` / `revokeObjectURL` are
//!   out of scope per the task brief.

use std::rc::Rc;

use crate::state::OpError;

#[allow(unused_imports)]
use zeroship_runtime_macros::{v8_class, v8_constructor, v8_getter, v8_method};

// ---------------------------------------------------------------------------
// Blob struct
// ---------------------------------------------------------------------------

/// A WHATWG Blob.
///
/// Holds an `Rc<Vec<u8>>` shared backing buffer plus a `(start, len)`
/// window into it. `slice()` returns a new Blob over the same `Rc`
/// with a narrower window — no byte copy.
pub struct Blob {
    /// Shared backing buffer. Constructed Blobs (via the user-visible
    /// `new Blob(...)`) get a fresh allocation; `slice()` shares the
    /// parent's buffer. The reference count keeps the backing alive
    /// across `slice()` chains and across native code passing handles
    /// in and out of V8.
    pub(crate) backing: Rc<Vec<u8>>,
    /// Inclusive start offset within `backing`.
    pub(crate) start: usize,
    /// Length of the window. `start + len <= backing.len()` is an
    /// invariant maintained by every constructor / mutator.
    pub(crate) len: usize,
    /// Lowercased MIME type per §3.1 step 2 of the constructor; empty
    /// string if any character is outside U+0020..U+007E.
    pub(crate) type_: String,
}

impl Blob {
    /// Borrow the byte slice this Blob represents. Useful for native
    /// callers and the inheriting `File` class.
    pub fn as_bytes(&self) -> &[u8] {
        &self.backing[self.start..self.start + self.len]
    }

    /// Internal constructor used by `slice()` and `File`'s parent build.
    /// Asserts the window invariants in debug builds.
    pub(crate) fn from_window(backing: Rc<Vec<u8>>, start: usize, len: usize, type_: String) -> Self {
        debug_assert!(start.saturating_add(len) <= backing.len());
        Blob { backing, start, len, type_ }
    }

    /// Build from a freshly-owned byte vector. Used when constructing
    /// a brand-new Blob (constructor path).
    pub(crate) fn from_bytes_owned(bytes: Vec<u8>, type_: String) -> Self {
        let len = bytes.len();
        Blob {
            backing: Rc::new(bytes),
            start: 0,
            len,
            type_,
        }
    }
}

impl Default for Blob {
    fn default() -> Self {
        Blob {
            backing: Rc::new(Vec::new()),
            start: 0,
            len: 0,
            type_: String::new(),
        }
    }
}

// ---------------------------------------------------------------------------
// Spec helpers
// ---------------------------------------------------------------------------

/// WebIDL `[Clamp] long long` conversion (§3.2.4): map a JS Number to
/// an i64 with banker's rounding (round-half-to-even). Used by
/// `Blob.slice(start, end)` per the WPT slice tests, which check that
/// `slice(1.5)` resolves to `slice(2)`, `slice(2.5)` also to `slice(2)`,
/// and `slice(3.5)` to `slice(4)`.
///
/// Spec algorithm (simplified for our integer-saturating use):
///   1. If x is NaN: return 0.
///   2. Set x = clamp(x, i64::MIN, i64::MAX).
///   3. Set x = round-half-to-even(x).
///   4. Return x.
fn clamp_long_long(x: f64) -> i64 {
    if x.is_nan() {
        return 0;
    }
    if x <= i64::MIN as f64 {
        return i64::MIN;
    }
    if x >= i64::MAX as f64 {
        return i64::MAX;
    }
    // f64::round_ties_even is round-half-to-even (banker's rounding).
    // Stable since 1.77 — checked compatible with our toolchain.
    x.round_ties_even() as i64
}

/// §3.1 step 2 of the Blob constructor: lowercase the `type` if every
/// character is a printable ASCII byte (U+0020..U+007E inclusive).
/// Otherwise the resulting type is the empty string. (V8 strings can
/// hold arbitrary UTF-16 code units; our printable check is on each
/// 16-bit code unit's value, which is correct: an astral non-printable
/// character produces non-printable code units in UTF-16 too.)
fn normalize_type(s: &str) -> String {
    if s.chars().all(|c| matches!(c as u32, 0x20..=0x7E)) {
        s.to_ascii_lowercase()
    } else {
        String::new()
    }
}

/// Public re-export of `normalize_type` for the `file.rs` sibling
/// module so it can apply the same MIME normalization rules without
/// duplicating the algorithm.
pub(crate) fn normalize_type_public(s: &str) -> String {
    normalize_type(s)
}

/// Public re-export of `clamp_long_long` for `file.rs::slice`.
pub(crate) fn clamp_long_long_public(x: f64) -> i64 {
    clamp_long_long(x)
}

/// Public wrapper around `Blob::from_bytes_owned`. Used by `File`'s
/// constructor.
pub(crate) fn from_bytes_owned_public(bytes: Vec<u8>, type_: String) -> Blob {
    Blob::from_bytes_owned(bytes, type_)
}

/// Public wrapper that walks an iterable of BlobParts and returns the
/// concatenated bytes. Used by `File`'s constructor.
pub(crate) fn collect_parts_public(
    scope: &mut v8::PinScope,
    parts: v8::Local<v8::Value>,
) -> Result<Vec<u8>, OpError> {
    let mut out = Vec::new();
    collect_parts(scope, parts, &mut out)?;
    Ok(out)
}

/// Public wrapper for `build_blob_stream`. Used by `File.stream()`.
pub(crate) fn build_blob_stream_public<'s>(
    scope: &mut v8::PinScope<'s, '_>,
    bytes: &[u8],
) -> v8::Local<'s, v8::Value> {
    build_blob_stream(scope, bytes)
}

/// Append the bytes of one BlobPart to `out`. Returns Err on a part
/// that isn't a valid `(BufferSource | Blob | USVString)` per WebIDL
/// §3.2.2 (sequence<BlobPart>) — though in practice WebIDL converts
/// non-conforming parts via ToString, which we mirror by treating
/// "anything that isn't a BufferSource and isn't a Blob" as a string.
///
/// Per spec §3.2 step 1.b, BlobPart bytes are *copied* — a Blob part
/// does NOT share its backing with the new Blob. We honour that
/// (conservatively): the new Blob owns a fresh contiguous buffer.
fn append_part_bytes(
    scope: &mut v8::PinScope,
    val: v8::Local<v8::Value>,
    out: &mut Vec<u8>,
) -> Result<(), OpError> {
    // BlobPart variant 1: Blob (or File via inheritance).
    // We detect Blob by reading internal field 0 and seeing if the
    // External resolves to our Box<Blob>. We can't do a strict type
    // check without storing a brand; the brand IS the External pointer
    // existing on a known internal-field slot. To minimise false hits,
    // we additionally check the prototype chain via the instanceof
    // operation against globalThis.Blob.
    if let Ok(obj) = v8::Local::<v8::Object>::try_from(val) {
        if is_blob_instance(scope, obj) {
            // Read the boxed Blob bytes via internal field 0.
            if let Some(ext) = obj
                .get_internal_field(scope, 0)
                .and_then(|v| v8::Local::<v8::External>::try_from(v).ok())
            {
                let ptr = ext.value() as *const Blob;
                if !ptr.is_null() {
                    // SAFETY: the External was set during construction
                    // to a Box<Blob>; the box stays alive while V8
                    // holds the wrapper, and we don't keep the
                    // reference past this scope.
                    let blob: &Blob = unsafe { &*ptr };
                    out.extend_from_slice(blob.as_bytes());
                    return Ok(());
                }
            }
        }
    }

    // BlobPart variant 2: BufferSource.
    if let Ok(view) = v8::Local::<v8::ArrayBufferView>::try_from(val) {
        // Buffer-views can be detached; copy_contents handles that
        // gracefully (zeros the destination).
        let mut buf = vec![0u8; view.byte_length()];
        view.copy_contents(&mut buf);
        out.extend_from_slice(&buf);
        return Ok(());
    }
    if let Ok(ab) = v8::Local::<v8::ArrayBuffer>::try_from(val) {
        let store = ab.get_backing_store();
        let n = ab.byte_length();
        let start = out.len();
        out.resize(start + n, 0);
        for i in 0..n {
            out[start + i] = store[i].get();
        }
        return Ok(());
    }

    // BlobPart variant 3: USVString. Per WebIDL §3.2.10, USVString
    // conversion is ToString(V) then replace unpaired surrogates with
    // U+FFFD. V8's `to_rust_string_lossy` performs that substitution
    // automatically (UTF-8 conversion of WTF-16 with U+FFFD on lone
    // surrogates).
    let s = val.to_rust_string_lossy(scope);
    out.extend_from_slice(s.as_bytes());
    Ok(())
}

/// True for objects and functions; matches WebIDL "object" semantics
/// (which excludes primitives but includes callable functions).
fn is_object_like(v: v8::Local<v8::Value>) -> bool {
    v.is_object() || v.is_function()
}

/// True if `obj` is an instance of `globalThis.Blob` per ES `instanceof`.
/// Used as the brand check inside the `BlobPart` union dispatch — we
/// can't rely solely on internal-field 0 being an External (other
/// classes use the same slot pattern), so we additionally verify the
/// prototype chain.
fn is_blob_instance(scope: &mut v8::PinScope, obj: v8::Local<v8::Object>) -> bool {
    let global = scope.get_current_context().global(scope);
    let key = match v8::String::new(scope, "Blob") {
        Some(s) => s,
        None => return false,
    };
    let class_v = match global.get(scope, key.into()) {
        Some(v) => v,
        None => return false,
    };
    let class_obj: v8::Local<v8::Object> = match class_v.try_into() {
        Ok(o) => o,
        Err(_) => return false,
    };
    obj.instance_of(scope, class_obj).unwrap_or(false)
}

// ---------------------------------------------------------------------------
// Constructor argument parsing
// ---------------------------------------------------------------------------

/// Parse the BlobPropertyBag (`{ type?: string, endings?: "transparent" | "native" }`).
/// Per WebIDL §3.2.18 dictionary conversion:
///   - `undefined` → empty dict (defaults applied).
///   - `null` → empty dict (defaults applied).
///   - **Object** (incl. function, regex, array, etc.) → read each member.
///   - Any other primitive (boolean, number, bigint, string, symbol)
///     → throw TypeError. The Blob WPT confirms this: passing 123,
///     123.4, true, 'abc' for options throws.
///
/// Members are accessed in **lexicographic key order** per spec
/// (`endings` before `type` for BlobPropertyBag). The WPT test
/// `"options properties should be accessed in lexicographic order"`
/// enforces this with throwing accessors that record their order.
fn parse_property_bag(
    scope: &mut v8::PinScope,
    init: v8::Local<v8::Value>,
) -> Result<String, OpError> {
    // Spec step 1: undefined/null → empty dictionary.
    if init.is_undefined() || init.is_null() {
        return Ok(String::new());
    }
    // Spec step 2: non-object primitives throw TypeError.
    if !is_object_like(init) {
        return Err(OpError::type_error(
            "Blob options must be an object or undefined",
        ));
    }
    let obj: v8::Local<v8::Object> = match init.try_into() {
        Ok(o) => o,
        // Defensive: the is_object_like check above implies this
        // succeeds, but the type system can't see that.
        Err(_) => return Ok(String::new()),
    };

    // Step 1: `endings` (observed but discarded — Linux has nothing
    // to normalize line endings against, and the spec lets us treat
    // "transparent" as the only effective option).
    //
    // Read happens BEFORE `type` per WebIDL lexicographic ordering.
    // We must do the get even though we don't use the result, because
    // the side-effect (a throwing accessor) is observable — the spec
    // mandates the throw propagate through Blob's constructor.
    let endings_key = v8::String::new(scope, "endings").unwrap();
    let endings_v = obj
        .get(scope, endings_key.into())
        .ok_or_else(|| OpError::type_error("Blob options.endings access threw"))?;
    if !endings_v.is_undefined() {
        // Spec also calls ToString on the value (per the EndingType
        // enum conversion). Trigger that side-effect.
        let _ = endings_v.to_rust_string_lossy(scope);
    }

    // Step 2: `type` member (USVString, default "").
    let type_key = v8::String::new(scope, "type").unwrap();
    let type_v = obj
        .get(scope, type_key.into())
        .ok_or_else(|| OpError::type_error("Blob options.type access threw"))?;
    let raw_type = if type_v.is_undefined() {
        String::new()
    } else {
        type_v.to_rust_string_lossy(scope)
    };

    Ok(normalize_type(&raw_type))
}

/// Walk a JS sequence (anything iterable) and collect bytes into `out`.
/// Returns Err if the value isn't iterable per WebIDL §3.2.18.
///
/// The constructor algorithm in §3.2 step 1 says:
///   "If invoked with zero parameters, the size MUST be 0…"
///   "Otherwise, for each blobPart in blobParts, the bytes…"
/// — i.e. the input is a `sequence<BlobPart>`, which in WebIDL means
/// "anything iterable". Arrays and TypedArray itself both qualify.
///
/// Only `undefined` is treated as default-empty; `null`, primitives,
/// and non-iterable objects all throw TypeError per WebIDL §3.2.18.
/// The Blob WPT `Blob constructor` checks this explicitly.
fn collect_parts(
    scope: &mut v8::PinScope,
    parts: v8::Local<v8::Value>,
    out: &mut Vec<u8>,
) -> Result<(), OpError> {
    if parts.is_undefined() {
        // The default-empty case. Spec §3.2 step 1: missing argument
        // means empty.
        return Ok(());
    }

    let parts_obj: v8::Local<v8::Object> = parts
        .try_into()
        .map_err(|_| OpError::type_error("Blob parts must be a sequence (iterable object)"))?;

    // Dispatch via @@iterator, like WebIDL sequence<T> does.
    let sym_iter = v8::Symbol::get_iterator(scope);
    let iter_fn_v = parts_obj
        .get(scope, sym_iter.into())
        .ok_or_else(|| OpError::type_error("@@iterator access threw"))?;
    if iter_fn_v.is_null_or_undefined() {
        return Err(OpError::type_error(
            "Blob parts must be a sequence (iterable object)",
        ));
    }
    let iter_fn: v8::Local<v8::Function> = iter_fn_v
        .try_into()
        .map_err(|_| OpError::type_error("@@iterator is not a function"))?;
    let iter_v = iter_fn
        .call(scope, parts_obj.into(), &[])
        .ok_or_else(|| OpError::type_error("@@iterator threw"))?;
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
        append_part_bytes(scope, value, out)?;
    }
    Ok(())
}

// ---------------------------------------------------------------------------
// Blob IDL surface
// ---------------------------------------------------------------------------

#[v8_class]
impl Blob {
    /// `new Blob(blobParts?: sequence<BlobPart>, options?: BlobPropertyBag)`
    /// per https://w3c.github.io/FileAPI/#constructorBlob.
    #[v8_constructor]
    fn new(
        scope: &mut v8::PinScope,
        parts: v8::Local<v8::Value>,
        options: v8::Local<v8::Value>,
    ) -> Result<Self, OpError> {
        let type_ = parse_property_bag(scope, options)?;
        let mut bytes: Vec<u8> = Vec::new();
        collect_parts(scope, parts, &mut bytes)?;
        Ok(Blob::from_bytes_owned(bytes, type_))
    }

    /// `size` getter — §3.1.
    /// Implemented as `f64` because JS numbers don't have a u64 type
    /// and Vec lengths fit comfortably in 53 bits anyway.
    #[v8_getter]
    fn size(&self) -> f64 {
        self.len as f64
    }

    /// `type` getter — §3.1.
    /// Renamed at JS surface from `type_` to `type` because the latter
    /// is a Rust keyword.
    #[v8_getter]
    #[v8_name = "type"]
    fn type_(&self) -> String {
        self.type_.clone()
    }

    /// `slice(start?: i32, end?: i32, contentType?: USVString) -> Blob`
    /// per §3.3.6. Negative indices clamp from the end; the resulting
    /// Blob shares the backing buffer with `self` (no copy).
    ///
    /// `start` and `end` are WebIDL `[Clamp] long long` arguments —
    /// non-integer values round to the nearest integer, with halves
    /// rounded to even (banker's rounding) per WebIDL §3.2.4. So
    /// `slice(1.5)` resolves to `slice(2)`, `slice(2.5)` is also
    /// `slice(2)`, and `slice(3.5)` is `slice(4)`.
    ///
    /// Args declared as `v8::Local<v8::Value>` so we can distinguish
    /// "missing" (undefined) from "0", which the spec treats
    /// differently for `start`/`end`.
    #[v8_method]
    fn slice<'s>(
        &self,
        scope: &mut v8::PinScope<'s, '_>,
        start_arg: v8::Local<v8::Value>,
        end_arg: v8::Local<v8::Value>,
        content_type_arg: v8::Local<v8::Value>,
    ) -> v8::Local<'s, v8::Value> {
        let size = self.len as i64;

        // start: optional [Clamp] long long; default 0.
        let start: i64 = if start_arg.is_undefined() {
            0
        } else {
            clamp_long_long(start_arg.number_value(scope).unwrap_or(0.0))
        };
        let rel_start = if start < 0 {
            (size + start).max(0)
        } else {
            start.min(size)
        };

        // end: optional [Clamp] long long; default size.
        let end: i64 = if end_arg.is_undefined() {
            size
        } else {
            clamp_long_long(end_arg.number_value(scope).unwrap_or(0.0))
        };
        let rel_end = if end < 0 {
            (size + end).max(0)
        } else {
            end.min(size)
        };

        let span = (rel_end - rel_start).max(0) as usize;
        let new_start = self.start + rel_start as usize;

        // contentType: optional USVString, default "" → empty type.
        // Spec §3.3.6 step 5: "Let relativeContentType be empty.
        // If contentType is given, let relativeContentType be the
        // result of normalize…". "Normalize" matches §3.1 step 2:
        // lowercase if all bytes are 0x20..=0x7E.
        let content_type = if content_type_arg.is_undefined() {
            String::new()
        } else {
            normalize_type(&content_type_arg.to_rust_string_lossy(scope))
        };

        // Build a new Blob sharing our backing.
        let new_blob = Blob::from_window(
            Rc::clone(&self.backing),
            new_start,
            span,
            content_type,
        );
        wrap_blob_in_v8(scope, new_blob)
    }

    /// `text() -> Promise<USVString>` — §3.3.8.
    /// Decodes the bytes as UTF-8 with U+FFFD substitution for invalid
    /// sequences (Rust's `String::from_utf8_lossy` matches WHATWG's
    /// "UTF-8 decode" algorithm).
    #[v8_method]
    fn text<'s>(&self, scope: &mut v8::PinScope<'s, '_>) -> v8::Local<'s, v8::Value> {
        let s = String::from_utf8_lossy(self.as_bytes()).into_owned();
        let resolver = v8::PromiseResolver::new(scope).unwrap();
        let promise = resolver.get_promise(scope);
        let v = v8::String::new(scope, &s).unwrap();
        resolver.resolve(scope, v.into());
        promise.into()
    }

    /// `arrayBuffer() -> Promise<ArrayBuffer>` — §3.3.9. Copies bytes
    /// (the spec mandates a fresh ArrayBuffer per call so callers can
    /// transfer it without affecting the Blob).
    #[v8_method]
    #[allow(non_snake_case)]
    fn arrayBuffer<'s>(&self, scope: &mut v8::PinScope<'s, '_>) -> v8::Local<'s, v8::Value> {
        let bytes = self.as_bytes();
        let ab = v8::ArrayBuffer::new(scope, bytes.len());
        if !bytes.is_empty() {
            let store = ab.get_backing_store();
            for (i, &b) in bytes.iter().enumerate() {
                store[i].set(b);
            }
        }
        let resolver = v8::PromiseResolver::new(scope).unwrap();
        let promise = resolver.get_promise(scope);
        resolver.resolve(scope, ab.into());
        promise.into()
    }

    /// `bytes() -> Promise<Uint8Array>` — newer spec method (still in
    /// "current" ED of File API). Same body as `arrayBuffer` but wraps
    /// the result in a Uint8Array view.
    #[v8_method]
    fn bytes<'s>(&self, scope: &mut v8::PinScope<'s, '_>) -> v8::Local<'s, v8::Value> {
        let bytes = self.as_bytes();
        let n = bytes.len();
        let ab = v8::ArrayBuffer::new(scope, n);
        if n > 0 {
            let store = ab.get_backing_store();
            for (i, &b) in bytes.iter().enumerate() {
                store[i].set(b);
            }
        }
        let u8arr = v8::Uint8Array::new(scope, ab, 0, n).unwrap();
        let resolver = v8::PromiseResolver::new(scope).unwrap();
        let promise = resolver.get_promise(scope);
        resolver.resolve(scope, u8arr.into());
        promise.into()
    }

    /// `stream() -> ReadableStream` — §3.3.7. Returns a native
    /// ReadableStream that yields the Blob's bytes as a single
    /// Uint8Array chunk, then closes.
    ///
    /// Implementation note: rather than wiring a `NativeSource` (whose
    /// async pull driver is not yet running per `streams-native.md` §VII.5),
    /// we construct a JS-side `new ReadableStream({ start(c) { c.enqueue();
    /// c.close(); } })` whose start callback enqueues the bytes
    /// synchronously. This goes through the well-tested JS-source path
    /// in `set_up_readable_stream_default_controller_from_underlying_source`
    /// without requiring a Native-driver dispatch.
    ///
    /// Spec leaves chunk count unspecified — implementations may yield
    /// many small chunks. We emit one chunk for v1 (matches polyfill);
    /// chunk-splitting can be added later without breaking semantics.
    #[v8_method]
    fn stream<'s>(&self, scope: &mut v8::PinScope<'s, '_>) -> v8::Local<'s, v8::Value> {
        build_blob_stream(scope, self.as_bytes())
    }
}

// ---------------------------------------------------------------------------
// Build a ReadableStream for Blob.stream()
// ---------------------------------------------------------------------------

/// Construct a `ReadableStream` whose underlying source has a synchronous
/// `start(controller)` that enqueues the entire Blob as a single
/// Uint8Array chunk and then closes the stream.
///
/// Implementation detail: the Uint8Array is stored in a Box<v8::Global<...>>
/// and handed to the start callback via the FunctionTemplate's data slot
/// (an External). On callback invocation, we move the Global out of the
/// box, materialize a Local chunk, and drive the controller. A V8
/// finalizer on the start function reclaims the box if the stream is
/// GC'd before start ever runs (~16 bytes leaked otherwise).
fn build_blob_stream<'s>(
    scope: &mut v8::PinScope<'s, '_>,
    bytes: &[u8],
) -> v8::Local<'s, v8::Value> {
    // Step 1: copy bytes into a fresh Uint8Array (the stream consumer
    // can transfer the buffer; we don't want them to scribble on the
    // shared Blob backing).
    let n = bytes.len();
    let ab = v8::ArrayBuffer::new(scope, n);
    if n > 0 {
        let store = ab.get_backing_store();
        for (i, &b) in bytes.iter().enumerate() {
            store[i].set(b);
        }
    }
    let chunk = v8::Uint8Array::new(scope, ab, 0, n).unwrap();
    let chunk_global = v8::Global::new(scope, chunk);

    // Step 2: build the start callback's payload. We allocate a
    // `Box<Option<v8::Global<v8::Uint8Array>>>`. The Option lets the
    // callback take() the Global out and drop the box's Global so the
    // backing handle can release; the finalizer on the function only
    // needs to drop the box's allocation.
    let payload: Box<Option<v8::Global<v8::Uint8Array>>> = Box::new(Some(chunk_global));
    let raw_ptr = Box::into_raw(payload);
    let raw_addr = raw_ptr as usize;
    let data_ext = v8::External::new(scope, raw_ptr as *mut std::ffi::c_void);

    // Build the start function via FunctionTemplate.
    let start_tmpl = v8::FunctionTemplate::builder(blob_stream_start_callback)
        .data(data_ext.into())
        .build(scope);
    let start_fn = start_tmpl.get_function(scope).unwrap();

    // Install a finalizer on start_fn so the box is reclaimed if the
    // stream is GC'd before start runs. Box<Option<Global<...>>> is
    // dropped which transitively drops the Global.
    let weak = v8::Weak::with_guaranteed_finalizer(
        scope,
        start_fn,
        Box::new(move || unsafe {
            drop(Box::from_raw(
                raw_addr as *mut Option<v8::Global<v8::Uint8Array>>,
            ));
        }),
    );
    std::mem::forget(weak);

    // Step 3: build the underlying source `{ start: start_fn }` and call
    // `new ReadableStream(underlyingSource)`.
    let us = v8::Object::new(scope);
    let start_key = v8::String::new(scope, "start").unwrap();
    us.set(scope, start_key.into(), start_fn.into());

    let global = scope.get_current_context().global(scope);
    let rs_key = v8::String::new(scope, "ReadableStream").unwrap();
    let rs_class_v = global.get(scope, rs_key.into()).unwrap();
    let rs_class: v8::Local<v8::Function> = rs_class_v.try_into().unwrap();
    let stream = rs_class
        .new_instance(scope, &[us.into()])
        .expect("Blob.stream(): ReadableStream constructor threw");
    stream.into()
}

/// Callback for the underlying-source `start(controller)` of
/// `Blob.stream()`. Pulls the carried Uint8Array out of the box, calls
/// `controller.enqueue(chunk)` then `controller.close()`. If start has
/// already run (defensive — spec only calls it once), the Option is
/// None and we silently return.
fn blob_stream_start_callback(
    scope: &mut v8::PinScope,
    args: v8::FunctionCallbackArguments,
    _rv: v8::ReturnValue,
) {
    // Recover the payload box.
    let data = args.data();
    let ext: v8::Local<v8::External> = match data.try_into() {
        Ok(e) => e,
        Err(_) => return,
    };
    let raw = ext.value() as *mut Option<v8::Global<v8::Uint8Array>>;
    if raw.is_null() {
        return;
    }
    // SAFETY: `raw` came from `Box::into_raw` in `build_blob_stream`; the
    // finalizer we installed reclaims the box on GC. Here we only take
    // the inner Global by mutable reference — the box itself stays
    // alive (the finalizer still owns it) and the Option is set to None
    // so subsequent calls (which spec says won't happen) become no-ops.
    let chunk_local: v8::Local<v8::Uint8Array> = {
        let opt = unsafe { &mut *raw };
        let Some(g) = opt.take() else { return };
        v8::Local::new(scope, &g)
    };

    // controller is args.get(0).
    let controller_v = args.get(0);
    let controller_obj: v8::Local<v8::Object> = match controller_v.try_into() {
        Ok(o) => o,
        Err(_) => return,
    };

    // controller.enqueue(chunk).
    let enqueue_key = v8::String::new(scope, "enqueue").unwrap();
    if let Some(enqueue_v) = controller_obj.get(scope, enqueue_key.into()) {
        if let Ok(enqueue_fn) = v8::Local::<v8::Function>::try_from(enqueue_v) {
            let _ = enqueue_fn.call(scope, controller_obj.into(), &[chunk_local.into()]);
        }
    }
    // controller.close().
    let close_key = v8::String::new(scope, "close").unwrap();
    if let Some(close_v) = controller_obj.get(scope, close_key.into()) {
        if let Ok(close_fn) = v8::Local::<v8::Function>::try_from(close_v) {
            let _ = close_fn.call(scope, controller_obj.into(), &[]);
        }
    }
}

// ---------------------------------------------------------------------------
// External wrap helper — used by slice() to return a fresh Blob wrapper
// ---------------------------------------------------------------------------

/// Wrap a Rust-built `Blob` into a JS object whose prototype chain is
/// the user-visible `globalThis.Blob.prototype`. Used by `slice()` so
/// the returned object passes `instanceof Blob`.
pub(crate) fn wrap_blob_in_v8<'s>(
    scope: &mut v8::PinScope<'s, '_>,
    blob: Blob,
) -> v8::Local<'s, v8::Value> {
    let tmpl = Blob::install(scope);
    let inst_tmpl = tmpl.instance_template(scope);
    let obj = inst_tmpl.new_instance(scope).unwrap();

    // Wire the prototype to the user-visible class so `instanceof`
    // works and the prototype method set matches.
    let proto = global_class_prototype(scope, "Blob").unwrap_or_else(|| {
        let class_fn = tmpl.get_function(scope).unwrap();
        let proto_key = v8::String::new(scope, "prototype").unwrap();
        class_fn.get(scope, proto_key.into()).unwrap()
    });
    obj.set_prototype(scope, proto);

    // Box the Blob, store in internal field 0, install finalizer.
    let boxed = Box::new(blob);
    let raw_ptr = Box::into_raw(boxed);
    let raw_addr = raw_ptr as usize;
    let ext = v8::External::new(scope, raw_ptr as *mut std::ffi::c_void);
    obj.set_internal_field(0, ext.into());

    let weak = v8::Weak::with_guaranteed_finalizer(
        scope,
        obj,
        Box::new(move || unsafe {
            drop(Box::from_raw(raw_addr as *mut Blob));
        }),
    );
    std::mem::forget(weak);

    obj.into()
}

/// Look up `globalThis[name].prototype`. Mirrors the helper in
/// `streams/readable.rs`.
fn global_class_prototype<'s>(
    scope: &mut v8::PinScope<'s, '_>,
    name: &str,
) -> Option<v8::Local<'s, v8::Value>> {
    let global = scope.get_current_context().global(scope);
    let key = v8::String::new(scope, name)?;
    let class_v = global.get(scope, key.into())?;
    let class_obj = v8::Local::<v8::Object>::try_from(class_v).ok()?;
    let proto_key = v8::String::new(scope, "prototype")?;
    class_obj.get(scope, proto_key.into())
}
