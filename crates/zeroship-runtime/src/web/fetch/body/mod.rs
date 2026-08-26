//! Native Body model per WHATWG Fetch §3 (https://fetch.spec.whatwg.org/#body).
//!
//! This module hosts the **shared** machinery that both Request and
//! Response build on:
//!
//! - `BodyImpl` — the two-headed body: a native ReadableStream
//!   plus an optional source for redirect rewinding / cheap clone.
//! - `Body` — Rust trait implemented by Request and Response so
//!   `text` / `json` / `arrayBuffer` / `bytes` / `blob` / `formData`
//!   can be installed on each class's prototype via a single shared
//!   installer (per v2 fix Process-8 — NOT a V8 base class, just a
//!   shared Rust trait). Both are defined directly in this file
//!   (rather than a nested `body` submodule) to avoid
//!   `clippy::module_inception` (a submodule sharing its parent
//!   module's name).
//! - `extract::extract_body` — Fetch §3.2 "extract a body" with the
//!   v2 dispatch order fix (C-10) and the proper USVString conversion
//!   for string bodies (C-11).
//! - `consumers` — the 6 body consumer methods, including the v2
//!   error-shape fix: `json()` rejects with **SyntaxError**
//!   not TypeError, `arrayBuffer()` rejects with **RangeError** at
//!   2GB, etc.
//! - `body_stream` — `read_all_bytes` + `read_one_chunk` helpers that
//!   drive consumers through the streams' public reader API. Per
//!   design §III.4, consumers MUST flow through the JS-visible reader
//!   to honour spec lock checks (locked stream → TypeError).
//!
//! ## Single-source slot rule
//!
//! Body state is split between the BodyImpl struct (Rust-side: source,
//! length) and a private V8 symbol on the wrapper for the stream Global.
//! No state is duplicated. Spec slots that need JS identity (the body
//! ReadableStream returned by `request.body`) live in V8 storage; pure
//! data (the source bytes for clone) lives on the Rust side.

pub mod body_stream;
pub mod consumers;
pub mod extract;

pub use extract::extract_body;

use std::cell::RefCell;
use std::rc::Rc;

// ---------------------------------------------------------------------------
// BodySource — the cheap-clone snapshot
// ---------------------------------------------------------------------------

/// The original body input, kept on the Rust side as
/// `Rc<Vec<u8>>` so `clone()` is constant-time (refcount bump) and
/// redirect rewinding can re-hand bytes to the next hop without
/// allocating a fresh body each time.
///
/// `Stream` means "no source" — the body was constructed from a
/// ReadableStream, so it's not rewindable and `clone()` must tee
/// the stream.
#[derive(Debug, Clone)]
pub enum BodySource {
    /// Raw byte sequence (Uint8Array / ArrayBuffer / DataView body, or
    /// the bytes for a redirect-rewindable string body that's already
    /// been UTF-8-encoded). MIME defaults to none — the caller picks.
    Bytes(Rc<Vec<u8>>),
    /// Blob — the Rc is the byte sequence, the Option<String> is the
    /// Blob's `type` (MIME). When extracting a body from a Blob the
    /// MIME becomes the default Content-Type if init.headers didn't
    /// provide one.
    Blob(Rc<Vec<u8>>, Option<String>),
    /// URLSearchParams — already-serialized application/x-www-form-
    /// urlencoded bytes. The default MIME goes with this variant.
    UrlSearchParams(Rc<Vec<u8>>),
    /// FormData — already-serialized multipart bytes plus the boundary.
    /// The default MIME is `multipart/form-data; boundary=<boundary>`.
    FormData(Rc<Vec<u8>>, String),
    /// User passed a ReadableStream. Not rewindable; `clone()` tees.
    Stream,
}

impl BodySource {
    /// True if the source is rewindable (redirect can re-send).
    pub fn is_rewindable(&self) -> bool {
        !matches!(self, BodySource::Stream)
    }
}

// ---------------------------------------------------------------------------
// BodyImpl — Rust-side body state shared by Request and Response
// ---------------------------------------------------------------------------

/// The body state stored alongside a Request or
/// Response wrapper. The wrapper's V8 internal field 0 holds a
/// Box<RequestState> / Box<ResponseState> that owns the BodyImpl.
///
/// The stream is the SOURCE OF TRUTH for
/// "body bytes still to read" once a consumer starts. The `source`
/// is only consulted by extract_body / clone / redirect-rewind. The
/// `length` is read by Content-Length sets (extract_body sets it on
/// the request's headers when the source has a known size).
///
/// FIX B (perf): the `stream` field is a `RefCell` so we can lazily
/// materialize the JS ReadableStream wrapper on first observation of
/// `.body` — fetched responses get `stream: RefCell::new(None)` +
/// `source: Some(Bytes)`, and consumer fast paths drain the source
/// without ever building a stream. The body getter materializes one
/// only when JS code reads `response.body`.
#[allow(missing_debug_implementations)]
pub struct BodyImpl {
    /// JS-visible ReadableStream that exposes the body bytes. None
    /// means either:
    ///   - body is conceptually `null` (then `source` is None too), OR
    ///   - body has a rewindable source but the stream has not been
    ///     materialized yet (FIX B lazy-stream path).
    pub stream: RefCell<Option<v8::Global<v8::Object>>>,
    /// Cheap-clone source snapshot. None for null-body cases.
    pub source: Option<BodySource>,
    /// Known byte length for Content-Length headers. None when:
    ///   - body is null,
    ///   - body is a stream (length unknown),
    ///   - body is a FormData with file/blob entries (multipart length
    ///     depends on file sizes we don't pre-buffer in v1).
    pub length: Option<u64>,
}

impl Default for BodyImpl {
    fn default() -> Self {
        BodyImpl {
            stream: RefCell::new(None),
            source: None,
            length: None,
        }
    }
}

impl BodyImpl {
    /// Build an empty (null) body.
    pub fn null() -> Self {
        Self::default()
    }

    /// True iff this body is conceptually null (per Fetch §3.1, a body
    /// is null when `[[body]]` is null). For the FIX B lazy-stream
    /// case, a body with `stream: None` but `source: Some(...)` is
    /// NOT null — the stream just hasn't been materialized yet.
    pub fn is_null(&self) -> bool {
        self.stream.borrow().is_none() && self.source.is_none()
    }
}

// ---------------------------------------------------------------------------
// Body trait — shared methods for Request and Response
// ---------------------------------------------------------------------------

/// Per v2 fix Process-8: the Body mixin is a Rust trait, NOT a V8 base
/// class. Both Request and Response implement this and the consumer
/// methods (`text` / `json` / `arrayBuffer` / `bytes` / `blob` /
/// `formData`) install on each class's prototype via a single shared
/// installer in `consumers::install_body_methods_on_proto`.
///
/// The trait gives the consumers a uniform way to:
///   1. Look up the body state from a JS wrapper.
///   2. Get/set `bodyUsed` (per Fetch §3.5 step 1: "set this's body's
///      stream to disturbed"). We use the stream's `disturbed` slot
///      via the streams crate's public lock check.
///   3. Read the Content-Type header (for `blob()` MIME).
pub trait Body {
    /// Extract a `&BodyImpl` from the JS wrapper's state. Returns
    /// `None` if the receiver isn't an instance of this class.
    fn body_state<'a>(
        scope: &mut v8::PinScope,
        this: v8::Local<v8::Object>,
    ) -> Option<&'a BodyImpl>;

    /// Read the `Content-Type` header value, if any. Used by
    /// `blob()` to set the resulting Blob's MIME type.
    fn content_type(
        scope: &mut v8::PinScope,
        this: v8::Local<v8::Object>,
    ) -> Option<String>;
}
