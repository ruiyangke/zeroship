//! `ReadableStreamBYOBReader` — spec §3.5.
//!
//! IDL (§3.5):
//! ```webidl
//! [Exposed=*]
//! interface ReadableStreamBYOBReader {
//!   constructor(ReadableStream stream);
//!   Promise<ReadableStreamReadResult> read(ArrayBufferView view,
//!                                           optional ReadableStreamBYOBReaderReadOptions options = {});
//!   undefined releaseLock();
//! };
//! ReadableStreamBYOBReader includes ReadableStreamGenericReader;
//!
//! dictionary ReadableStreamBYOBReaderReadOptions {
//!   [EnforceRange] unsigned long long min = 1;
//! };
//! ```
//!
//! Storage (§XV per-class):
//! - `[[stream]]`              → V8 priv sym `[[stream]]`
//! - `[[closedPromise]]` + closedResolver → paired storage in ReadableStreamBYOBReader
//! - `[[readIntoRequests]]`    → Rust VecDeque on ReadableStreamBYOBReader

use std::cell::RefCell;
use std::collections::VecDeque;

use zeroship_runtime_macros::{v8_class, v8_constructor};

use crate::state::OpError;
use crate::streams::algorithms;
use crate::streams::pull_into::ViewConstructor;
use crate::streams::readable::{is_readable_stream, StreamState};
use crate::streams::slots::{self, CLOSED_PROMISE, READER, STORED_ERROR, STREAM};

// ---------------------------------------------------------------------------
// Reader state — Box<ReadableStreamBYOBReader> in internal field 0
// ---------------------------------------------------------------------------

/// Boxed state behind the JS `ReadableStreamBYOBReader` wrapper. Lives in
/// internal field 0; reclaimed by the V8 weak finalizer registered via the
/// `#[v8_class]` macro.
///
/// This parallels `ReadableStreamDefaultReader`. The
/// constructor (Self::new) validates the stream argument (must be a
/// byte-typed ReadableStream, must be unlocked) and stashes it in
/// `pending_stream` so the post_init hook can run ReaderGenericInitialize
/// + write the BYOB tag priv-sym after the box is reachable via field 0.
#[allow(missing_debug_implementations)]
pub struct ReadableStreamBYOBReader {
    pub read_into_requests: RefCell<VecDeque<ReadIntoRequest>>,
    pub closed_resolver: RefCell<Option<v8::Global<v8::PromiseResolver>>>,
    /// Stream stashed by the constructor body for `after_install`.
    /// `None` for readers built via `acquire_readable_stream_byob_reader`
    /// (the Rust-side helper handles GenericInitialize directly).
    pub pending_stream: RefCell<Option<v8::Global<v8::Object>>>,
}

impl ReadableStreamBYOBReader {
    /// Allocate the boxed state with no stashed stream — used by the
    /// `acquire_*` Rust helper which runs ReaderGenericInitialize directly
    /// rather than through the macro's post_init hook.
    fn new_for_internal(closed_resolver: v8::Global<v8::PromiseResolver>) -> Self {
        Self {
            read_into_requests: RefCell::new(VecDeque::new()),
            closed_resolver: RefCell::new(Some(closed_resolver)),
            pending_stream: RefCell::new(None),
        }
    }
}

#[v8_class]
#[v8_to_string_tag = "ReadableStreamBYOBReader"]
impl ReadableStreamBYOBReader {
    /// `new ReadableStreamBYOBReader(stream)` — spec §3.5.4 step 1–4.
    /// Validates the argument is a byte-typed ReadableStream that is not
    /// already locked. The PromiseResolver alloc + stream stash run here
    /// so the post_init hook (`after_install`) can finish wiring after
    /// the box is reachable via field 0.
    #[v8_constructor(post_init = "after_install")]
    fn new(
        scope: &mut v8::PinScope,
        stream: v8::Local<v8::Value>,
    ) -> Result<Self, OpError> {
        let stream = v8::Local::<v8::Object>::try_from(stream).map_err(|_| {
            OpError::type_error(
                "ReadableStreamBYOBReader: argument must be a ReadableStream",
            )
        })?;
        if !is_readable_stream(scope, stream) {
            return Err(OpError::type_error(
                "ReadableStreamBYOBReader: argument must be a ReadableStream",
            ));
        }
        // Reject byte-only stream check: BYOBReader is only valid on byte
        // streams (their controller is a ReadableByteStreamController).
        let controller_v = slots::read_slot(scope, stream, slots::CONTROLLER);
        let controller = v8::Local::<v8::Object>::try_from(controller_v).map_err(|_| {
            OpError::type_error("ReadableStreamBYOBReader: stream has no controller")
        })?;
        if !crate::streams::readable_byte_controller::is_byte_controller(scope, controller)
        {
            return Err(OpError::type_error(
                "ReadableStreamBYOBReader: cannot construct on a non-byte-stream",
            ));
        }
        if algorithms::is_readable_stream_locked(scope, stream) {
            return Err(OpError::type_error(
                "ReadableStreamBYOBReader: stream is already locked",
            ));
        }
        // V8 returns None from PromiseResolver::new only on isolate
        // termination or out-of-memory — surface as Error so the macro's
        // Err arm throws cleanly rather than panicking.
        let resolver = v8::PromiseResolver::new(scope)
            .ok_or_else(|| OpError::error("PromiseResolver::new failed"))?;
        let resolver_g = v8::Global::new(scope, resolver);
        let stream_g = v8::Global::new(scope, stream);
        Ok(Self {
            read_into_requests: RefCell::new(VecDeque::new()),
            closed_resolver: RefCell::new(Some(resolver_g)),
            pending_stream: RefCell::new(Some(stream_g)),
        })
    }

    /// `ReaderGenericInitialize(reader, stream)` + BYOB tag write — spec
    /// §3.9.2. Runs after the macro has installed the Box in field 0.
    ///
    /// CAUTION: do NOT call into JS inside the `with_state` closure (it
    /// holds `&Self` for the closure's duration; reentrant `&mut self`
    /// methods would alias). The closure body is pure RefCell mutation.
    pub(crate) fn after_install(
        scope: &mut v8::PinScope,
        this: v8::Local<v8::Object>,
    ) -> Result<(), OpError> {
        let (stream_g_opt, resolver_g_opt) = with_state(scope, this, |s| {
            (
                s.pending_stream.borrow_mut().take(),
                s.closed_resolver.borrow().clone(),
            )
        })
        .ok_or_else(|| OpError::error("after_install: with_state returned None"))?;
        let stream_g = stream_g_opt
            .ok_or_else(|| OpError::error("after_install: missing pending_stream"))?;
        let resolver_g = resolver_g_opt
            .ok_or_else(|| OpError::error("after_install: missing closed_resolver"))?;
        let stream = v8::Local::new(scope, &stream_g);
        let resolver = v8::Local::new(scope, &resolver_g);
        let closed_promise = resolver.get_promise(scope);
        finalize_byob_reader(scope, this, stream, closed_promise);
        Ok(())
    }
}

/// Read-into request — spec §3.5.4. The three step variants
/// (`chunkSteps`/`closeSteps`/`errorSteps`) are encoded by the dispatch
/// site (this module) which calls one of three methods on the request.
#[allow(missing_debug_implementations)]
pub struct ReadIntoRequest {
    pub kind: ReadIntoRequestKind,
}

pub enum ReadIntoRequestKind {
    /// User-facing `reader.read(view)` — resolve a promise resolver with
    /// `{value, done}`.
    Js {
        resolver: v8::Global<v8::PromiseResolver>,
    },
    /// Internal Rust callback — analogous to ReadRequestKind::Native.
    /// Currently unused but reserved for byte-tee and future internal
    /// users (compression-stream, fetch body bridge).
    #[allow(dead_code)]
    Native(Box<dyn ReadIntoRequestNative + 'static>),
}

pub trait ReadIntoRequestNative: 'static {
    fn chunk_steps<'s>(
        self: Box<Self>,
        scope: &mut v8::PinScope<'s, '_>,
        chunk: v8::Local<'s, v8::Value>,
    );
    fn close_steps<'s>(
        self: Box<Self>,
        scope: &mut v8::PinScope<'s, '_>,
        chunk: v8::Local<'s, v8::Value>,
    );
    fn error_steps<'s>(
        self: Box<Self>,
        scope: &mut v8::PinScope<'s, '_>,
        reason: v8::Local<'s, v8::Value>,
    );
}

// ---------------------------------------------------------------------------
// V8 wrapper helpers
// ---------------------------------------------------------------------------

/// Discriminator for `is_byob_reader` — set as a priv-sym tag at
/// construction so we can distinguish from default readers (whose
/// internal field 0 is also non-null).
const BYOB_READER_TAG_SLOT: &str = "[[byobReader.tag]]";

pub fn is_byob_reader(scope: &mut v8::PinScope, obj: v8::Local<v8::Object>) -> bool {
    if obj
        .get_internal_field(scope, 0)
        .and_then(|v| v8::Local::<v8::External>::try_from(v).ok())
        .map(|e| e.value().is_null())
        .unwrap_or(true)
    {
        return false;
    }
    !slots::slot_is_empty(scope, obj, BYOB_READER_TAG_SLOT)
}

pub fn with_state<R>(
    scope: &mut v8::PinScope,
    reader: v8::Local<v8::Object>,
    f: impl FnOnce(&ReadableStreamBYOBReader) -> R,
) -> Option<R> {
    let raw = reader.get_internal_field(scope, 0)?;
    let ext = v8::Local::<v8::External>::try_from(raw).ok()?;
    let ptr = ext.value() as *const ReadableStreamBYOBReader;
    if ptr.is_null() {
        return None;
    }
    // SAFETY: External points at a Box<ReadableStreamBYOBReader>; dropped
    // only by the V8 weak finalizer.
    let inst = unsafe { &*ptr };
    Some(f(inst))
}

// ---------------------------------------------------------------------------
// Cross-class operations on the stream side (called by byte controller)
// ---------------------------------------------------------------------------

/// `ReadableStreamHasBYOBReader(stream)` — §3.9.1.14. Real
/// implementation that replaces the always-false stub. Used by the byte
/// controller's `ShouldCallPull`.
pub fn readable_stream_has_byob_reader(
    scope: &mut v8::PinScope,
    stream: v8::Local<v8::Object>,
) -> bool {
    let reader_v = slots::read_slot(scope, stream, READER);
    let Ok(reader) = v8::Local::<v8::Object>::try_from(reader_v) else {
        return false;
    };
    is_byob_reader(scope, reader)
}

/// Variant for debug_assert sites that take a controller obj. The
/// `scope.x.y` access is OK in this dispatch since the controller's
/// streamObj is a separate priv-sym; this helper exists so the byte
/// controller's debug_asserts compile cleanly.
pub fn readable_stream_has_byob_reader_check(
    scope: &mut v8::PinScope,
    stream: v8::Local<v8::Object>,
) -> bool {
    readable_stream_has_byob_reader(scope, stream)
}

/// `ReadableStreamGetNumReadIntoRequests(stream)` — §3.9.1.x.
pub fn readable_stream_get_num_read_into_requests(
    scope: &mut v8::PinScope,
    stream: v8::Local<v8::Object>,
) -> usize {
    let reader_v = slots::read_slot(scope, stream, READER);
    let Ok(reader) = v8::Local::<v8::Object>::try_from(reader_v) else {
        return 0;
    };
    if !is_byob_reader(scope, reader) {
        return 0;
    }
    with_state(scope, reader, |s| s.read_into_requests.borrow().len()).unwrap_or(0)
}

/// `ReadableStreamFulfillReadIntoRequest(stream, chunk, done)` — §3.9.1.x.
pub fn readable_stream_fulfill_read_into_request<'s>(
    scope: &mut v8::PinScope<'s, '_>,
    stream: v8::Local<v8::Object>,
    chunk: v8::Local<'s, v8::Value>,
    done: bool,
) {
    let reader_v = slots::read_slot(scope, stream, READER);
    let Ok(reader) = v8::Local::<v8::Object>::try_from(reader_v) else {
        return;
    };
    let req = with_state(scope, reader, |s| s.read_into_requests.borrow_mut().pop_front())
        .flatten();
    let Some(req) = req else { return };
    if done {
        // Spec closeSteps: pass `chunk` (the partial-fill view) to
        // closeSteps; resolve {value: chunk, done: true}.
        fulfill_read_into_close(scope, req, chunk);
    } else {
        fulfill_read_into_chunk(scope, req, chunk);
    }
}

/// Add a read-into request to the BYOB reader's queue.
pub fn add_read_into_request(
    scope: &mut v8::PinScope,
    stream: v8::Local<v8::Object>,
    request: ReadIntoRequest,
) {
    let reader_v = slots::read_slot(scope, stream, READER);
    let Ok(reader) = v8::Local::<v8::Object>::try_from(reader_v) else {
        return;
    };
    with_state(scope, reader, |s| {
        s.read_into_requests.borrow_mut().push_back(request);
    });
}

/// Resolve a single read-into request as `{value: view, done: true}`.
/// Used when a BYOB read happens on a closed stream.
pub fn resolve_read_into_request_done<'s>(
    scope: &mut v8::PinScope<'s, '_>,
    request: ReadIntoRequest,
    chunk: v8::Local<'s, v8::Value>,
) {
    fulfill_read_into_close(scope, request, chunk);
}

/// Resolve a single read-into request as `{value: view, done: false}`.
/// Used by `pull_into`'s queue-fast-path: the bytes came from the
/// queue, so the read is non-final regardless of subsequent close.
pub fn resolve_read_into_request_chunk<'s>(
    scope: &mut v8::PinScope<'s, '_>,
    request: ReadIntoRequest,
    chunk: v8::Local<'s, v8::Value>,
) {
    fulfill_read_into_chunk(scope, request, chunk);
}

/// Reject a single pending read-into request with the given error.
pub fn error_read_into_request<'s>(
    scope: &mut v8::PinScope<'s, '_>,
    request: ReadIntoRequest,
    error: v8::Local<'s, v8::Value>,
) {
    fulfill_read_into_error(scope, request, error);
}

// ---------------------------------------------------------------------------
// Internal "AcquireReadableStreamBYOBReader" path — Rust-side construction
// ---------------------------------------------------------------------------

/// `getReader({mode: "byob"})` Rust path. Mirrors
/// `acquire_readable_stream_default_reader` shape: build the wrapper via
/// the macro-emitted FunctionTemplate's `new_instance`, set prototype,
/// then run the box install + ReaderGenericInitialize manually. The JS
/// `[[Construct]]` path (`new ReadableStreamBYOBReader(stream)`) goes
/// through the macro instead.
pub fn acquire_readable_stream_byob_reader<'s>(
    scope: &mut v8::PinScope<'s, '_>,
    stream: v8::Local<v8::Object>,
) -> Result<v8::Local<'s, v8::Object>, String> {
    if algorithms::is_readable_stream_locked(scope, stream) {
        return Err("ReadableStream.getReader: stream is already locked".to_string());
    }
    let controller_v = slots::read_slot(scope, stream, slots::CONTROLLER);
    let Ok(controller) = v8::Local::<v8::Object>::try_from(controller_v) else {
        return Err("getReader(byob): stream has no controller".to_string());
    };
    if !crate::streams::readable_byte_controller::is_byte_controller(scope, controller) {
        return Err(
            "getReader(byob): can only be called on a ReadableStream with type=\"bytes\""
                .to_string(),
        );
    }
    // The macro's `install` is idempotent and isolate-cached, so it
    // returns the same FunctionTemplate as the global `install_global`
    // path — which means `instanceof ReadableStreamBYOBReader` works
    // either with or without the streams namespace installed (the prior
    // hand-rolled defensive globalThis lookup is no longer needed).
    let tmpl = ReadableStreamBYOBReader::install(scope);
    let inst_tmpl = tmpl.instance_template(scope);
    let reader_obj = inst_tmpl
        .new_instance(scope)
        .ok_or_else(|| "alloc BYOB reader instance".to_string())?;
    let class_fn = tmpl.get_function(scope).unwrap();
    let proto_key = v8::String::new(scope, "prototype").unwrap();
    let proto_v = class_fn.get(scope, proto_key.into()).unwrap();
    reader_obj.set_prototype(scope, proto_v);

    set_up_byob_reader_internal(scope, reader_obj, stream);
    Ok(reader_obj)
}

/// Box install + BYOB tag write + ReaderGenericInitialize for the
/// Rust-side (`acquire_*`) path.
fn set_up_byob_reader_internal(
    scope: &mut v8::PinScope,
    reader: v8::Local<v8::Object>,
    stream: v8::Local<v8::Object>,
) {
    let resolver = v8::PromiseResolver::new(scope).unwrap();
    let closed_promise = resolver.get_promise(scope);
    let resolver_g = v8::Global::new(scope, resolver);

    let state = ReadableStreamBYOBReader::new_for_internal(resolver_g);
    let boxed = Box::new(state);
    let raw_ptr = Box::into_raw(boxed);
    let raw_addr = raw_ptr as usize;
    let ext = v8::External::new(scope, raw_ptr as *mut std::ffi::c_void);
    reader.set_internal_field(0, ext.into());
    let weak = v8::Weak::with_guaranteed_finalizer(
        scope,
        reader,
        Box::new(move || unsafe {
            drop(Box::from_raw(raw_addr as *mut ReadableStreamBYOBReader));
        }),
    );
    std::mem::forget(weak);

    finalize_byob_reader(scope, reader, stream, closed_promise);
}

/// BYOB tag priv-sym + ReaderGenericInitialize. Shared between the JS
/// path's `after_install` hook and the Rust `set_up_byob_reader_internal`
/// path. Runs after the box has been installed in field 0.
fn finalize_byob_reader<'s>(
    scope: &mut v8::PinScope<'s, '_>,
    reader: v8::Local<v8::Object>,
    stream: v8::Local<v8::Object>,
    closed_promise: v8::Local<'s, v8::Promise>,
) {
    // BYOB reader tag (used by is_byob_reader).
    let tag = v8::Boolean::new(scope, true);
    slots::write_slot(scope, reader, BYOB_READER_TAG_SLOT, tag.into());

    // ReaderGenericInitialize.
    slots::write_slot(scope, reader, STREAM, stream.into());
    slots::write_slot(scope, stream, READER, reader.into());
    slots::write_slot(scope, reader, CLOSED_PROMISE, closed_promise.into());

    let st = match crate::streams::readable::with_rs_state(scope, stream, |s| s.state.get()) {
        Some(s) => s,
        None => return,
    };
    match st {
        StreamState::Readable => {}
        StreamState::Closed => {
            resolve_closed_promise(scope, reader);
        }
        StreamState::Errored => {
            let stored = slots::read_slot(scope, stream, STORED_ERROR);
            reject_closed_promise(scope, reader, stored);
        }
    }
}

// ---------------------------------------------------------------------------
// Closed-promise helpers (mirror default-reader)
// ---------------------------------------------------------------------------

pub fn resolve_closed_promise(scope: &mut v8::PinScope, reader: v8::Local<v8::Object>) {
    let resolver_g =
        with_state(scope, reader, |s| s.closed_resolver.borrow_mut().take()).flatten();
    if let Some(resolver_g) = resolver_g {
        let resolver = v8::Local::new(scope, &resolver_g);
        let undef = v8::undefined(scope);
        resolver.resolve(scope, undef.into());
    }
}

pub fn reject_closed_promise<'s>(
    scope: &mut v8::PinScope<'s, '_>,
    reader: v8::Local<v8::Object>,
    error: v8::Local<'s, v8::Value>,
) {
    let resolver_g =
        with_state(scope, reader, |s| s.closed_resolver.borrow_mut().take()).flatten();
    if let Some(resolver_g) = resolver_g {
        let resolver = v8::Local::new(scope, &resolver_g);
        resolver.reject(scope, error);
        let cp_v = slots::read_slot(scope, reader, CLOSED_PROMISE);
        if let Ok(cp) = v8::Local::<v8::Promise>::try_from(cp_v) {
            crate::streams::promise_resolve::set_promise_is_handled_to_true(scope, cp);
        }
    }
}

// ---------------------------------------------------------------------------
// IDL methods
// ---------------------------------------------------------------------------

fn closed_getter_callback(
    scope: &mut v8::PinScope,
    args: v8::FunctionCallbackArguments,
    mut rv: v8::ReturnValue,
) {
    let this = args.this();
    if !is_byob_reader(scope, this) {
        let msg = v8::String::new(scope, "closed: receiver invalid").unwrap();
        let exc = v8::Exception::type_error(scope, msg);
        let resolver = v8::PromiseResolver::new(scope).unwrap();
        let p = resolver.get_promise(scope);
        resolver.reject(scope, exc);
        rv.set(p.into());
        return;
    }
    rv.set(slots::read_slot(scope, this, CLOSED_PROMISE));
}

fn read_method_callback<'s>(
    scope: &mut v8::PinScope<'s, '_>,
    args: v8::FunctionCallbackArguments<'s>,
    mut rv: v8::ReturnValue<'s>,
) {
    let this = args.this();
    if !is_byob_reader(scope, this) {
        let msg = v8::String::new(scope, "read: receiver invalid").unwrap();
        let exc = v8::Exception::type_error(scope, msg);
        let resolver = v8::PromiseResolver::new(scope).unwrap();
        let p = resolver.get_promise(scope);
        resolver.reject(scope, exc);
        rv.set(p.into());
        return;
    }
    let view_v = args.get(0);
    let Ok(view) = v8::Local::<v8::ArrayBufferView>::try_from(view_v) else {
        let msg = v8::String::new(scope, "read: argument must be an ArrayBufferView").unwrap();
        let exc = v8::Exception::type_error(scope, msg);
        let resolver = v8::PromiseResolver::new(scope).unwrap();
        let p = resolver.get_promise(scope);
        resolver.reject(scope, exc);
        rv.set(p.into());
        return;
    };
    if view.byte_length() == 0 {
        let msg = v8::String::new(scope, "read: view byteLength is 0").unwrap();
        let exc = v8::Exception::type_error(scope, msg);
        let resolver = v8::PromiseResolver::new(scope).unwrap();
        let p = resolver.get_promise(scope);
        resolver.reject(scope, exc);
        rv.set(p.into());
        return;
    }
    // Detached check on view.buffer.
    let buffer = match view.buffer(scope) {
        Some(b) => b,
        None => {
            let msg = v8::String::new(scope, "read: view has no buffer").unwrap();
            let exc = v8::Exception::type_error(scope, msg);
            let resolver = v8::PromiseResolver::new(scope).unwrap();
            let p = resolver.get_promise(scope);
            resolver.reject(scope, exc);
            rv.set(p.into());
            return;
        }
    };
    if buffer.was_detached() {
        let msg = v8::String::new(scope, "read: view's buffer is detached").unwrap();
        let exc = v8::Exception::type_error(scope, msg);
        let resolver = v8::PromiseResolver::new(scope).unwrap();
        let p = resolver.get_promise(scope);
        resolver.reject(scope, exc);
        rv.set(p.into());
        return;
    }
    if buffer.byte_length() == 0 {
        let msg = v8::String::new(scope, "read: view's buffer byteLength is 0").unwrap();
        let exc = v8::Exception::type_error(scope, msg);
        let resolver = v8::PromiseResolver::new(scope).unwrap();
        let p = resolver.get_promise(scope);
        resolver.reject(scope, exc);
        rv.set(p.into());
        return;
    }
    let ctor = match ViewConstructor::from_view(view) {
        Some(c) => c,
        None => {
            let msg = v8::String::new(scope, "read: unrecognized view constructor").unwrap();
            let exc = v8::Exception::type_error(scope, msg);
            let resolver = v8::PromiseResolver::new(scope).unwrap();
            let p = resolver.get_promise(scope);
            resolver.reject(scope, exc);
            rv.set(p.into());
            return;
        }
    };
    let elem_size = ctor.element_size();
    // Parse options (min, default 1).
    let opts = args.get(1);
    let min = match parse_min(scope, opts) {
        Ok(n) => n,
        Err(exc) => {
            let resolver = v8::PromiseResolver::new(scope).unwrap();
            let p = resolver.get_promise(scope);
            resolver.reject(scope, exc);
            rv.set(p.into());
            return;
        }
    };
    if min == 0 {
        let msg = v8::String::new(scope, "read: min must be greater than 0").unwrap();
        let exc = v8::Exception::type_error(scope, msg);
        let resolver = v8::PromiseResolver::new(scope).unwrap();
        let p = resolver.get_promise(scope);
        resolver.reject(scope, exc);
        rv.set(p.into());
        return;
    }
    let elem_count = (view.byte_length() as u64) / elem_size;
    if min > elem_count {
        let msg = v8::String::new(scope, "read: min exceeds view length").unwrap();
        let exc = v8::Exception::range_error(scope, msg);
        let resolver = v8::PromiseResolver::new(scope).unwrap();
        let p = resolver.get_promise(scope);
        resolver.reject(scope, exc);
        rv.set(p.into());
        return;
    }
    // Stream must be present.
    let stream_v = slots::read_slot(scope, this, STREAM);
    let Ok(stream) = v8::Local::<v8::Object>::try_from(stream_v) else {
        let msg = v8::String::new(scope, "read: reader has no stream").unwrap();
        let exc = v8::Exception::type_error(scope, msg);
        let resolver = v8::PromiseResolver::new(scope).unwrap();
        let p = resolver.get_promise(scope);
        resolver.reject(scope, exc);
        rv.set(p.into());
        return;
    };
    // Set [[disturbed]] = true.
    crate::streams::readable::with_rs_state(scope, stream, |s| s.disturbed.set(true));

    // Allocate result Promise.
    let resolver = v8::PromiseResolver::new(scope).unwrap();
    let p = resolver.get_promise(scope);
    let resolver_g = v8::Global::new(scope, resolver);

    // If state == "errored", reject with stored error.
    let st = crate::streams::readable::with_rs_state(scope, stream, |s| s.state.get())
        .unwrap_or(StreamState::Errored);
    if st == StreamState::Errored {
        let stored = slots::read_slot(scope, stream, STORED_ERROR);
        let resolver_l = v8::Local::new(scope, &resolver_g);
        resolver_l.reject(scope, stored);
        rv.set(p.into());
        return;
    }

    // Pull-into bytes_written = min * elementSize.
    let minimum_fill = min * elem_size;
    let request = ReadIntoRequest {
        kind: ReadIntoRequestKind::Js {
            resolver: resolver_g,
        },
    };
    let controller_v = slots::read_slot(scope, stream, slots::CONTROLLER);
    if let Ok(controller) = v8::Local::<v8::Object>::try_from(controller_v) {
        crate::streams::readable_byte_controller::readable_byte_stream_controller_pull_into(
            scope,
            controller,
            view,
            minimum_fill,
            request,
        );
    }
    rv.set(p.into());
}

fn parse_min<'s>(
    scope: &mut v8::PinScope<'s, '_>,
    opts: v8::Local<v8::Value>,
) -> Result<u64, v8::Local<'s, v8::Value>> {
    if opts.is_undefined() || opts.is_null() {
        return Ok(1);
    }
    let Ok(obj) = v8::Local::<v8::Object>::try_from(opts) else {
        let msg = v8::String::new(scope, "read: options must be an object").unwrap();
        let exc = v8::Exception::type_error(scope, msg);
        return Err(exc);
    };
    let key = v8::String::new(scope, "min").unwrap();
    let v = obj
        .get(scope, key.into())
        .unwrap_or_else(|| v8::undefined(scope).into());
    if v.is_undefined() {
        return Ok(1);
    }
    // [EnforceRange] unsigned long long: convert to integer; if NaN/<0/
    // > 2^53-1 → throw RangeError.
    let n = v
        .number_value(scope)
        .ok_or_else(|| {
            let msg = v8::String::new(scope, "read: min must be a number").unwrap();
            let exc = v8::Exception::type_error(scope, msg);
            exc
        })?;
    // [EnforceRange] per WebIDL: if not a finite integer in the valid
    // range, throw a TypeError. (NaN/inf, < 0, > 2^53-1 → TypeError.)
    if !n.is_finite() || n < 0.0 || n > (1u64 << 53) as f64 || n.fract() != 0.0 {
        let msg = v8::String::new(scope, "read: min out of [EnforceRange]").unwrap();
        let exc = v8::Exception::type_error(scope, msg);
        return Err(exc);
    }
    Ok(n as u64)
}

fn release_lock_method_callback(
    scope: &mut v8::PinScope,
    args: v8::FunctionCallbackArguments,
    _rv: v8::ReturnValue,
) {
    let this = args.this();
    if !is_byob_reader(scope, this) {
        let msg = v8::String::new(scope, "releaseLock: receiver invalid").unwrap();
        let exc = v8::Exception::type_error(scope, msg);
        scope.throw_exception(exc);
        return;
    }
    readable_stream_byob_reader_release(scope, this);
}

/// `ReadableStreamBYOBReaderRelease(reader)` — §3.5.x.
pub fn readable_stream_byob_reader_release(
    scope: &mut v8::PinScope,
    reader: v8::Local<v8::Object>,
) {
    let stream_v = slots::read_slot(scope, reader, STREAM);
    if stream_v.is_undefined() {
        return;
    }
    // Tell the byte controller to mark the front descriptor's
    // readerType = None.
    if let Ok(stream) = v8::Local::<v8::Object>::try_from(stream_v) {
        crate::streams::readable_byte_controller::release_steps(scope, stream);
    }
    readable_stream_byob_reader_generic_release(scope, reader);
    let msg = v8::String::new(
        scope,
        "Reader released; outstanding read() requests rejected",
    )
    .unwrap();
    let exc = v8::Exception::type_error(scope, msg);
    let exc_l = exc.into();
    readable_stream_byob_reader_error_read_into_requests(scope, reader, exc_l);
}

/// `ReadableStreamBYOBReaderErrorReadIntoRequests(reader, e)` — §3.5.x.
pub fn readable_stream_byob_reader_error_read_into_requests<'s>(
    scope: &mut v8::PinScope<'s, '_>,
    reader: v8::Local<v8::Object>,
    error: v8::Local<'s, v8::Value>,
) {
    let drained: Vec<ReadIntoRequest> = with_state(scope, reader, |state| {
        state.read_into_requests.borrow_mut().drain(..).collect()
    })
    .unwrap_or_default();
    for req in drained {
        fulfill_read_into_error(scope, req, error);
    }
}

/// Generic release — mirrors default reader's logic.
fn readable_stream_byob_reader_generic_release(
    scope: &mut v8::PinScope,
    reader: v8::Local<v8::Object>,
) {
    let stream_v = slots::read_slot(scope, reader, STREAM);
    let Ok(stream) = v8::Local::<v8::Object>::try_from(stream_v) else {
        return;
    };
    let st = crate::streams::readable::with_rs_state(scope, stream, |s| s.state.get());
    let msg = v8::String::new(
        scope,
        "Reader was released and can no longer be used to monitor the stream's state",
    )
    .unwrap();
    let exc = v8::Exception::type_error(scope, msg);
    match st {
        Some(StreamState::Readable) => {
            reject_closed_promise(scope, reader, exc);
        }
        _ => {
            let resolver = v8::PromiseResolver::new(scope).unwrap();
            resolver.reject(scope, exc);
            let p = resolver.get_promise(scope);
            slots::write_slot(scope, reader, CLOSED_PROMISE, p.into());
            crate::streams::promise_resolve::set_promise_is_handled_to_true(scope, p);
        }
    }
    slots::delete_slot(scope, stream, READER);
    slots::delete_slot(scope, reader, STREAM);
}

fn cancel_method_callback<'s>(
    scope: &mut v8::PinScope<'s, '_>,
    args: v8::FunctionCallbackArguments<'s>,
    mut rv: v8::ReturnValue<'s>,
) {
    let this = args.this();
    if !is_byob_reader(scope, this) {
        let msg = v8::String::new(scope, "cancel: receiver invalid").unwrap();
        let exc = v8::Exception::type_error(scope, msg);
        let resolver = v8::PromiseResolver::new(scope).unwrap();
        let p = resolver.get_promise(scope);
        resolver.reject(scope, exc);
        rv.set(p.into());
        return;
    }
    let stream_v = slots::read_slot(scope, this, STREAM);
    let Ok(stream) = v8::Local::<v8::Object>::try_from(stream_v) else {
        let msg = v8::String::new(scope, "cancel: reader has no associated stream").unwrap();
        let exc = v8::Exception::type_error(scope, msg);
        let resolver = v8::PromiseResolver::new(scope).unwrap();
        let p = resolver.get_promise(scope);
        resolver.reject(scope, exc);
        rv.set(p.into());
        return;
    };
    let reason = args.get(0);
    let promise = algorithms::readable_stream_cancel(scope, stream, reason);
    rv.set(promise.into());
}

// ---------------------------------------------------------------------------
// Read-into request fulfillment
// ---------------------------------------------------------------------------

fn fulfill_read_into_chunk<'s>(
    scope: &mut v8::PinScope<'s, '_>,
    request: ReadIntoRequest,
    chunk: v8::Local<'s, v8::Value>,
) {
    match request.kind {
        ReadIntoRequestKind::Js { resolver } => {
            let resolver_l = v8::Local::new(scope, &resolver);
            let result = v8::Object::new(scope);
            let value_key = v8::String::new(scope, "value").unwrap();
            let done_key = v8::String::new(scope, "done").unwrap();
            result.set(scope, value_key.into(), chunk);
            result.set(scope, done_key.into(), v8::Boolean::new(scope, false).into());
            resolver_l.resolve(scope, result.into());
        }
        ReadIntoRequestKind::Native(req) => {
            req.chunk_steps(scope, chunk);
        }
    }
}

fn fulfill_read_into_close<'s>(
    scope: &mut v8::PinScope<'s, '_>,
    request: ReadIntoRequest,
    chunk: v8::Local<'s, v8::Value>,
) {
    match request.kind {
        ReadIntoRequestKind::Js { resolver } => {
            let resolver_l = v8::Local::new(scope, &resolver);
            let result = v8::Object::new(scope);
            let value_key = v8::String::new(scope, "value").unwrap();
            let done_key = v8::String::new(scope, "done").unwrap();
            result.set(scope, value_key.into(), chunk);
            result.set(scope, done_key.into(), v8::Boolean::new(scope, true).into());
            resolver_l.resolve(scope, result.into());
        }
        ReadIntoRequestKind::Native(req) => {
            req.close_steps(scope, chunk);
        }
    }
}

fn fulfill_read_into_error<'s>(
    scope: &mut v8::PinScope<'s, '_>,
    request: ReadIntoRequest,
    error: v8::Local<'s, v8::Value>,
) {
    match request.kind {
        ReadIntoRequestKind::Js { resolver } => {
            let resolver_l = v8::Local::new(scope, &resolver);
            resolver_l.reject(scope, error);
        }
        ReadIntoRequestKind::Native(req) => {
            req.error_steps(scope, error);
        }
    }
}

// ---------------------------------------------------------------------------
// Public install
// ---------------------------------------------------------------------------

pub fn install(scope: &mut v8::PinScope, global: v8::Local<v8::Object>) {
    // Macro-emitted FunctionTemplate carries the constructor + Symbol.toStringTag.
    let tmpl = ReadableStreamBYOBReader::install(scope);

    // Patch in the IDL methods on the prototype (read / releaseLock /
    // cancel / closed). They stay raw FunctionCallbacks for the same
    // reason as the default reader (need direct args.this() + Promise
    // alloc). See the headers.rs `install_global` pattern for prior art.
    let class_fn = tmpl.get_function(scope).unwrap();
    let proto_key = v8::String::new(scope, "prototype").unwrap();
    let proto_v = class_fn.get(scope, proto_key.into()).unwrap();
    let proto: v8::Local<v8::Object> = proto_v.try_into().unwrap();

    // closed getter
    {
        let closed_key = v8::String::new(scope, "closed").unwrap();
        let getter_tmpl = v8::FunctionTemplate::new(scope, closed_getter_callback);
        let getter_fn = getter_tmpl.get_function(scope).unwrap();
        let mut desc = v8::PropertyDescriptor::new_from_get_set(
            getter_fn.into(),
            v8::undefined(scope).into(),
        );
        desc.set_configurable(true);
        desc.set_enumerable(true);
        proto.define_property(scope, closed_key.into(), &desc);
    }

    // read / releaseLock / cancel
    install_proto_method_on_object(scope, proto, "read", read_method_callback);
    install_proto_method_on_object(scope, proto, "releaseLock", release_lock_method_callback);
    install_proto_method_on_object(scope, proto, "cancel", cancel_method_callback);

    let key = v8::String::new(scope, "ReadableStreamBYOBReader").unwrap();
    global.set(scope, key.into(), class_fn.into());
}

fn install_proto_method_on_object(
    scope: &mut v8::PinScope,
    proto: v8::Local<v8::Object>,
    name: &str,
    cb: impl v8::MapFnTo<v8::FunctionCallback>,
) {
    let key = v8::String::new(scope, name).unwrap();
    let tmpl = v8::FunctionTemplate::new(scope, cb);
    let func = tmpl.get_function(scope).unwrap();
    proto.set(scope, key.into(), func.into());
}
