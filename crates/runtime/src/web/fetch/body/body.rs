//! `BodyImpl`, `BodySource`, and the shared `Body` trait.
//!
//! The two-headed body model holds:
//!
//! - **`stream`** — a JS-visible ReadableStream Global. Returned from
//!   `request.body` / `response.body`. Locked / disturbed semantics
//!   are tracked by the stream itself (so a `getReader()` call from
//!   user code disturbs the body in a way the consumer methods see).
//! - **`source`** — an optional cheap-to-clone snapshot of the original
//!   body bytes (or the typed parts that produced them). Used for:
//!     - **Redirect rewinding**: when fetch redirects via 307/308, the
//!       request body must be re-sent. The stream is one-shot, so we
//!       re-extract from `source` at each hop.
//!     - **Clone**: `request.clone()` / `response.clone()` tee the
//!       stream and clone the source via Rc — both halves usable
//!       independently, no double consumption.
//! - **`length`** — the body's known byte length for Content-Length,
//!   if computable up front. None for ReadableStream-backed bodies and
//!   for FormData entries that include Blob/File parts (the length
//!   depends on multipart serialization which we defer in v1).

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
