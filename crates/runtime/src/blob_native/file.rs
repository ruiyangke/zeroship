//! Native `File` per WHATWG File API §4 — https://w3c.github.io/FileAPI/#file-section.
//!
//! Inherits from `Blob` (the prototype chain is wired so
//! `file instanceof Blob === true`). Adds the `name` and
//! `lastModified` getters.
//!
//! ## Storage
//!
//! Composition: `File` carries a `Blob` field plus its own `name` and
//! `last_modified`. We could subclass via `#[v8_inherit(Blob)]`, but
//! the macro doesn't yet support inheritance (`v8_class.rs` line 26-27
//! lists this as a known gap). Instead we handle inheritance manually
//! in `install`:
//!   1. Install `File` as its own class with its own constructor.
//!   2. Set `File.prototype.__proto__ = Blob.prototype` so all Blob
//!      methods resolve via prototype lookup.
//!   3. Override the size/type/slice/text/arrayBuffer/bytes/stream
//!      getters so they read from the inner Blob — which means File's
//!      own prototype gets the same methods as Blob (delegated to the
//!      inner Blob via the same `internal field 0` shape).
//!
//! Critical detail: because the macro stores File's box in internal
//! field 0, but Blob methods (looked up via prototype chain) read from
//! field 0 expecting a `Blob`, **File's box layout must be
//! pointer-compatible with Blob's read path**. We achieve this with
//! `#[repr(C)]` on File and placing the Blob field FIRST. The Blob
//! method callback then reinterprets `*mut Blob` from the same address
//! and reads only the Blob portion — ignoring the trailing `name` and
//! `last_modified`.
//!
//! That trick is the same one the spec references when it says "File
//! IS-A Blob": we make the in-memory representations compatible at the
//! prefix.

use std::rc::Rc;

use crate::blob_native::blob::Blob;
use crate::state::OpError;

#[allow(unused_imports)]
use zeroship_runtime_macros::{v8_class, v8_constructor, v8_getter, v8_method};

// ---------------------------------------------------------------------------
// File struct — Blob prefix + extra fields
// ---------------------------------------------------------------------------

/// `#[repr(C)]` so the layout is fixed: a `Blob` followed by `name`
/// and `last_modified`. Code that has a `*mut File` and a `*mut Blob`
/// can use the same address — the leading bytes form a valid Blob.
///
/// This is required because the prototype-inherited Blob methods
/// (size/type/slice/text/etc.) access internal field 0 and cast it as
/// `*mut Blob`. With `#[repr(C)]` we guarantee that field offsets are
/// the source-order ones, and the Blob lives at offset 0.
#[repr(C)]
pub struct File {
    /// Inherited Blob state. **Must be first.** Code that walks the
    /// prototype chain reaches Blob's getter callbacks, which cast
    /// internal-field-0 to `*mut Blob` and dereference at offset 0.
    pub(crate) blob: Blob,
    /// `name` per §4.1.2: USVString — the filename. Spec mandates we
    /// replace U+002F SOLIDUS ("/") with U+003A COLON (":") in the
    /// final name (preventing path-injection in form-data); see §4.3
    /// step 4.
    pub(crate) name: String,
    /// `lastModified` per §4.1.4: long long, milliseconds since
    /// Unix epoch. Default is "the current time" if the user's
    /// FilePropertyBag doesn't carry one (§4.3 step 5).
    pub(crate) last_modified: i64,
}

impl Default for File {
    fn default() -> Self {
        File {
            blob: Blob::default(),
            name: String::new(),
            last_modified: 0,
        }
    }
}

// ---------------------------------------------------------------------------
// FilePropertyBag parsing
// ---------------------------------------------------------------------------

/// FilePropertyBag inherits BlobPropertyBag and adds `lastModified`.
/// Returns `(type, last_modified)`.
fn parse_file_property_bag(
    scope: &mut v8::PinScope,
    init: v8::Local<v8::Value>,
) -> Result<(String, i64), OpError> {
    if init.is_undefined() || init.is_null() {
        return Ok((String::new(), current_time_ms()));
    }
    let obj: v8::Local<v8::Object> = match init.try_into() {
        Ok(o) => o,
        Err(_) => return Err(OpError::type_error("File options must be an object")),
    };

    // type member.
    let type_key = v8::String::new(scope, "type").unwrap();
    let type_v = obj
        .get(scope, type_key.into())
        .ok_or_else(|| OpError::type_error("File options.type access threw"))?;
    let raw_type = if type_v.is_undefined() {
        String::new()
    } else {
        type_v.to_rust_string_lossy(scope)
    };
    let type_ = crate::blob_native::blob::normalize_type_public(&raw_type);

    // lastModified member: long long, default = current time.
    let lm_key = v8::String::new(scope, "lastModified").unwrap();
    let lm_v = obj
        .get(scope, lm_key.into())
        .ok_or_else(|| OpError::type_error("File options.lastModified access threw"))?;
    let last_modified: i64 = if lm_v.is_undefined() {
        current_time_ms()
    } else {
        // WebIDL `long long`: ToInt64(V). Step over NaN/±∞ via
        // number_value().unwrap_or(0.0); JS truncates toward zero.
        lm_v.number_value(scope).unwrap_or(0.0) as i64
    };

    // Honour `endings` for symmetry with Blob — read the value (so a
    // throwing accessor would surface), discard it (no-op on Linux).
    let endings_key = v8::String::new(scope, "endings").unwrap();
    let _ = obj.get(scope, endings_key.into());

    Ok((type_, last_modified))
}

/// Current wall-clock time in milliseconds since Unix epoch.
fn current_time_ms() -> i64 {
    use std::time::{SystemTime, UNIX_EPOCH};
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis() as i64)
        .unwrap_or(0)
}

// ---------------------------------------------------------------------------
// File IDL surface
// ---------------------------------------------------------------------------

#[v8_class]
impl File {
    /// `new File(fileBits, fileName, options?)` per §4.3.
    /// Args: sequence<BlobPart> fileBits, USVString fileName,
    ///       FilePropertyBag options.
    ///
    /// Per WebIDL, both `fileBits` and `fileName` are required (no
    /// `optional` keyword in the IDL); calling `new File()` or
    /// `new File([])` throws TypeError. We detect "missing" by
    /// `is_undefined()` — JS callers passing `undefined` explicitly
    /// or omitting the arg both end up the same.
    #[v8_constructor]
    fn new(
        scope: &mut v8::PinScope,
        file_bits: v8::Local<v8::Value>,
        file_name: v8::Local<v8::Value>,
        options: v8::Local<v8::Value>,
    ) -> Result<Self, OpError> {
        if file_bits.is_undefined() {
            return Err(OpError::type_error(
                "Failed to construct 'File': 1 argument required, but only 0 present.",
            ));
        }
        if file_name.is_undefined() {
            return Err(OpError::type_error(
                "Failed to construct 'File': 2 arguments required, but only 1 present.",
            ));
        }

        // Step 2: name = USVString(fileName). USVString conversion runs
        // ToString(V) then replaces unpaired surrogates with U+FFFD;
        // V8's `to_rust_string_lossy` does this (we treat WTF-16 lone
        // surrogates as U+FFFD via UTF-8 transcoding).
        //
        // The spec doesn't actually require slash→colon replacement at
        // the constructor level — that's in §4 step 4 of the html
        // form-data section. Per File API constructor §4.3 step 2, we
        // just take the string as-is.
        let name = file_name.to_rust_string_lossy(scope);

        let (type_, last_modified) = parse_file_property_bag(scope, options)?;

        // Build the Blob portion via the same algorithm Blob's
        // constructor uses. We re-use Blob's helpers via the public
        // `parse_parts_for_file` thin wrapper.
        let bytes = crate::blob_native::blob::collect_parts_public(scope, file_bits)?;
        let blob = crate::blob_native::blob::from_bytes_owned_public(bytes, type_);

        Ok(File {
            blob,
            name,
            last_modified,
        })
    }

    /// `name` getter — §4.1.2.
    #[v8_getter]
    fn name(&self) -> String {
        self.name.clone()
    }

    /// `lastModified` getter — §4.1.4.
    /// Returned as f64 because JS Number can hold the full i64 range
    /// up to 2^53 (and we'll never have a meaningful timestamp past
    /// year 287396).
    #[v8_getter]
    #[allow(non_snake_case)]
    fn lastModified(&self) -> f64 {
        self.last_modified as f64
    }

    /// `size` getter — duplicated here because we override the
    /// prototype chain with our own File.prototype, and we need
    /// `file.size` to read from the inner Blob. The Blob version on
    /// the parent prototype can't be invoked directly because methods
    /// resolve through the prototype chain via this-binding, and our
    /// own size getter takes precedence on `File.prototype`.
    #[v8_getter]
    fn size(&self) -> f64 {
        self.blob.as_bytes().len() as f64
    }

    /// `type` getter — see `size` rationale. Renamed at JS surface
    /// from `type_` to `type` because the latter is a Rust keyword.
    #[v8_getter]
    #[v8_name = "type"]
    fn type_(&self) -> String {
        self.blob.type_.clone()
    }

    /// `slice` — delegates to the inner Blob's slice algorithm. Returns
    /// a *Blob* (not a File) per spec §4.1.6: "When a slice is invoked
    /// on a File, it returns a new Blob".
    #[v8_method]
    fn slice<'s>(
        &self,
        scope: &mut v8::PinScope<'s, '_>,
        start_arg: v8::Local<v8::Value>,
        end_arg: v8::Local<v8::Value>,
        content_type_arg: v8::Local<v8::Value>,
    ) -> v8::Local<'s, v8::Value> {
        let size = self.blob.as_bytes().len() as i64;
        let start: i64 = if start_arg.is_undefined() {
            0
        } else {
            crate::blob_native::blob::clamp_long_long_public(
                start_arg.number_value(scope).unwrap_or(0.0),
            )
        };
        let rel_start = if start < 0 {
            (size + start).max(0)
        } else {
            start.min(size)
        };
        let end: i64 = if end_arg.is_undefined() {
            size
        } else {
            crate::blob_native::blob::clamp_long_long_public(
                end_arg.number_value(scope).unwrap_or(0.0),
            )
        };
        let rel_end = if end < 0 {
            (size + end).max(0)
        } else {
            end.min(size)
        };
        let span = (rel_end - rel_start).max(0) as usize;
        let new_start = self.blob.start + rel_start as usize;
        let content_type = if content_type_arg.is_undefined() {
            String::new()
        } else {
            crate::blob_native::blob::normalize_type_public(
                &content_type_arg.to_rust_string_lossy(scope),
            )
        };
        let new_blob = Blob::from_window(
            Rc::clone(&self.blob.backing),
            new_start,
            span,
            content_type,
        );
        crate::blob_native::blob::wrap_blob_in_v8(scope, new_blob)
    }

    /// `text() -> Promise<USVString>` — delegates to inner Blob bytes.
    #[v8_method]
    fn text<'s>(&self, scope: &mut v8::PinScope<'s, '_>) -> v8::Local<'s, v8::Value> {
        let s = String::from_utf8_lossy(self.blob.as_bytes()).into_owned();
        let resolver = v8::PromiseResolver::new(scope).unwrap();
        let promise = resolver.get_promise(scope);
        let v = v8::String::new(scope, &s).unwrap();
        resolver.resolve(scope, v.into());
        promise.into()
    }

    /// `arrayBuffer() -> Promise<ArrayBuffer>` — delegates to inner Blob.
    #[v8_method]
    #[allow(non_snake_case)]
    fn arrayBuffer<'s>(&self, scope: &mut v8::PinScope<'s, '_>) -> v8::Local<'s, v8::Value> {
        let bytes = self.blob.as_bytes();
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

    /// `bytes() -> Promise<Uint8Array>` — delegates to inner Blob.
    #[v8_method]
    fn bytes<'s>(&self, scope: &mut v8::PinScope<'s, '_>) -> v8::Local<'s, v8::Value> {
        let bytes = self.blob.as_bytes();
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

    /// `stream() -> ReadableStream` — delegates to inner Blob bytes.
    #[v8_method]
    fn stream<'s>(&self, scope: &mut v8::PinScope<'s, '_>) -> v8::Local<'s, v8::Value> {
        crate::blob_native::blob::build_blob_stream_public(scope, self.blob.as_bytes())
    }
}
