//! `ReadableStreamDefaultReader` — spec §3.4.
//!
//! IDL (§3.4.1):
//! ```webidl
//! [Exposed=*]
//! interface ReadableStreamDefaultReader {
//!   constructor(ReadableStream stream);
//!   Promise<ReadableStreamReadResult> read();
//!   undefined releaseLock();
//! };
//! ReadableStreamDefaultReader includes ReadableStreamGenericReader;
//!
//! interface mixin ReadableStreamGenericReader {
//!   readonly attribute Promise<undefined> closed;
//!   Promise<undefined> cancel(optional any reason);
//! };
//! ```
//!
//! Storage (§XV per-class):
//! - `[[stream]]`        → V8 priv sym `[[stream]]` on the reader wrapper
//! - `[[closedPromise]]` + closedResolver → paired storage in the reader's RustState
//!   (the closed-Promise getter reads the priv sym `[[closedPromise]]`; the
//!   resolver lives in the Rust state for resolve/reject access).
//! - `[[readRequests]]`  → Rust VecDeque on the reader's RustState (per
//!   spec §3.4.5 the queue lives on the reader; we follow the spec
//!   exactly. `algorithms.rs` reaches in via `with_state` helper.)

use std::cell::RefCell;
use std::collections::VecDeque;

use zeroship_runtime_macros::{v8_class, v8_constructor};

use crate::state::OpError;
use crate::streams::algorithms;
use crate::streams::readable::{is_readable_stream, StreamState};
use crate::streams::slots::{self, CLOSED_PROMISE, CONTROLLER, READER, STORED_ERROR, STREAM};

// ---------------------------------------------------------------------------
// Reader state — Box<ReadableStreamDefaultReader> in internal field 0
// ---------------------------------------------------------------------------

/// Boxed state behind the JS `ReadableStreamDefaultReader` wrapper. Lives in
/// internal field 0; reclaimed by the V8 weak finalizer registered via the
/// `#[v8_class]` macro.
///
/// MAC-02 migration: `#[v8_class] + #[v8_constructor(post_init = ...)]`
/// drives box install. The constructor body (Self::new) validates the
/// stream argument and stashes it in `pending_stream` so the post_init
/// hook can run `ReaderGenericInitialize` after the box is reachable via
/// field 0.
#[allow(missing_debug_implementations)]
pub struct ReadableStreamDefaultReader {
    /// Outstanding read requests (FIFO).
    pub read_requests: RefCell<VecDeque<ReadRequest>>,
    /// Resolver for the reader's `[[closedPromise]]`. The Promise itself
    /// is stored in the reader wrapper's V8 priv sym `[[closedPromise]]`.
    pub closed_resolver: RefCell<Option<v8::Global<v8::PromiseResolver>>>,
    /// Stream stashed by the constructor body for `after_install` to wire
    /// `reader.[[stream]]` / `stream.[[reader]]` / closedPromise. Cleared
    /// (`take()`) inside `after_install`. Carrying it through the box (vs.
    /// re-fetching from JS args, which the post_init hook can't see) is
    /// the §1.6 "args plumbing" pattern from the macro design.
    ///
    /// `None` for readers built via `acquire_readable_stream_default_reader`
    /// (the Rust-side helper handles GenericInitialize directly without
    /// going through post_init).
    pub pending_stream: RefCell<Option<v8::Global<v8::Object>>>,
}

impl ReadableStreamDefaultReader {
    /// Allocate the boxed state with no stashed stream — used by the
    /// `acquire_*` Rust helper which runs ReaderGenericInitialize directly
    /// rather than through the macro's post_init hook.
    fn new_for_internal(closed_resolver: v8::Global<v8::PromiseResolver>) -> Self {
        Self {
            read_requests: RefCell::new(VecDeque::new()),
            closed_resolver: RefCell::new(Some(closed_resolver)),
            pending_stream: RefCell::new(None),
        }
    }
}

#[v8_class]
#[v8_to_string_tag = "ReadableStreamDefaultReader"]
impl ReadableStreamDefaultReader {
    /// `new ReadableStreamDefaultReader(stream)` — spec §3.4.4 step 1–3.
    ///
    /// Receiver / lock checks fail with TypeError (must-new is emitted by
    /// the macro). The PromiseResolver alloc and stream stash run here so
    /// the post_init hook (`after_install`) can finish wiring after the
    /// box is reachable via field 0.
    #[v8_constructor(post_init = "after_install")]
    fn new(
        scope: &mut v8::PinScope,
        stream: v8::Local<v8::Value>,
    ) -> Result<Self, OpError> {
        let stream = v8::Local::<v8::Object>::try_from(stream).map_err(|_| {
            OpError::type_error(
                "ReadableStreamDefaultReader: argument must be a ReadableStream",
            )
        })?;
        if !is_readable_stream(scope, stream) {
            return Err(OpError::type_error(
                "ReadableStreamDefaultReader: argument must be a ReadableStream",
            ));
        }
        if algorithms::is_readable_stream_locked(scope, stream) {
            return Err(OpError::type_error(
                "ReadableStreamDefaultReader: stream is already locked",
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
            read_requests: RefCell::new(VecDeque::new()),
            closed_resolver: RefCell::new(Some(resolver_g)),
            pending_stream: RefCell::new(Some(stream_g)),
        })
    }

    /// `ReaderGenericInitialize(reader, stream)` — spec §3.9.2. Runs after
    /// the macro has installed the Box in internal field 0, so
    /// `with_state(scope, this, ...)` resolves the stashed
    /// `pending_stream` + `closed_resolver`.
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
        let stream_g = stream_g_opt.ok_or_else(|| {
            OpError::error("after_install: missing pending_stream")
        })?;
        let resolver_g = resolver_g_opt.ok_or_else(|| {
            OpError::error("after_install: missing closed_resolver")
        })?;
        let stream = v8::Local::new(scope, &stream_g);
        let resolver = v8::Local::new(scope, &resolver_g);
        let closed_promise = resolver.get_promise(scope);
        readable_stream_reader_generic_initialize(scope, this, stream, closed_promise);
        Ok(())
    }
}

/// Read request — spec §3.4.4. The three step variants
/// (`chunkSteps`/`closeSteps`/`errorSteps`) are encoded by the
/// dispatch site (`algorithms.rs`) which calls one of three methods on
/// the request.
#[allow(missing_debug_implementations)]
pub struct ReadRequest {
    pub kind: ReadRequestKind,
}

pub enum ReadRequestKind {
    /// User-facing `reader.read()` — resolve a promise resolver with
    /// `{value, done}`.
    Js {
        resolver: v8::Global<v8::PromiseResolver>,
    },
    /// Internal Rust callback — used by pipeTo and tee. The chunk/close/
    /// error steps run SYNCHRONOUSLY inside the controller's fulfill
    /// path, matching the spec's ReadRequest object semantics. This
    /// avoids the extra microtask hop that a resolver-based read
    /// introduces, which the spec relies on for handler ordering
    /// (per pipeStep "currentWrite must be set before the read
    /// resolves" — see WPT close-propagation-forward
    /// "shutdown must not occur until the final write completes;
    /// preventClose = true").
    Native(Box<dyn ReadRequestNative + 'static>),
}

/// Spec ReadRequest object: three step callbacks for chunk/close/error.
/// Implementors run synchronously inside the controller's fulfill path.
pub trait ReadRequestNative: 'static {
    fn chunk_steps<'s>(
        self: Box<Self>,
        scope: &mut v8::PinScope<'s, '_>,
        chunk: v8::Local<'s, v8::Value>,
    );
    fn close_steps(self: Box<Self>, scope: &mut v8::PinScope);
    fn error_steps<'s>(
        self: Box<Self>,
        scope: &mut v8::PinScope<'s, '_>,
        reason: v8::Local<'s, v8::Value>,
    );
}

// ---------------------------------------------------------------------------
// V8 wrapper helpers
// ---------------------------------------------------------------------------

pub fn is_default_reader(scope: &mut v8::PinScope, obj: v8::Local<v8::Object>) -> bool {
    let has_state = obj
        .get_internal_field(scope, 0)
        .and_then(|v| v8::Local::<v8::External>::try_from(v).ok())
        .map(|e| !e.value().is_null())
        .unwrap_or(false);
    if !has_state {
        return false;
    }
    // BYOB readers also have a non-null state Box; distinguish by the
    // BYOB-only tag slot.
    if crate::streams::readable_byob_reader::is_byob_reader(scope, obj) {
        return false;
    }
    true
}

pub fn with_state<R>(
    scope: &mut v8::PinScope,
    reader: v8::Local<v8::Object>,
    f: impl FnOnce(&ReadableStreamDefaultReader) -> R,
) -> Option<R> {
    let raw_v8_field = reader.get_internal_field(scope, 0)?;
    let ext = v8::Local::<v8::External>::try_from(raw_v8_field).ok()?;
    let ptr = ext.value() as *const ReadableStreamDefaultReader;
    if ptr.is_null() {
        return None;
    }
    // SAFETY: External points at a Box<ReadableStreamDefaultReader>; dropped
    // only by the V8 weak finalizer (registered by the macro for JS-built
    // readers, by `acquire_*` for Rust-built readers).
    let inst = unsafe { &*ptr };
    Some(f(inst))
}

// ---------------------------------------------------------------------------
// Internal "AcquireReadableStreamDefaultReader" path — Rust-side construction
// ---------------------------------------------------------------------------

/// Build a fresh ReadableStreamDefaultReader wrapper bound to `stream` from
/// Rust (i.e. without going through the JS `[[Construct]]` path). Used by
/// pipeTo / tee / asyncIterator / `getReader()` to mint a reader on a
/// stream the caller has already proven unlocked.
///
/// The macro-emitted constructor's box install + post_init only runs for
/// JS-side `new ReadableStreamDefaultReader(stream)`. Internal-only mints
/// allocate the Box manually here, mirroring `mint_abort_signal` in
/// `web/dom/abort_signal.rs` — the same pattern for "construct the wrapper
/// without re-entering the JS validation path".
pub fn acquire_readable_stream_default_reader<'s>(
    scope: &mut v8::PinScope<'s, '_>,
    stream: v8::Local<v8::Object>,
) -> Result<v8::Local<'s, v8::Object>, String> {
    if algorithms::is_readable_stream_locked(scope, stream) {
        return Err("ReadableStream.getReader: stream is already locked".to_string());
    }
    let tmpl = ReadableStreamDefaultReader::install(scope);
    let inst_tmpl = tmpl.instance_template(scope);
    let reader_obj = inst_tmpl
        .new_instance(scope)
        .ok_or_else(|| "alloc reader instance".to_string())?;
    // Wire prototype so the macro's brand check (prototype-chain walk) and
    // the patched-in raw method callbacks resolve.
    let class_fn = tmpl.get_function(scope).unwrap();
    let proto_key = v8::String::new(scope, "prototype").unwrap();
    let proto_v = class_fn.get(scope, proto_key.into()).unwrap();
    reader_obj.set_prototype(scope, proto_v);

    set_up_default_reader_internal(scope, reader_obj, stream);
    Ok(reader_obj)
}

/// Box install + ReaderGenericInitialize for the Rust-side (`acquire_*`)
/// path. Mirrors what the macro's box-install + `after_install` hook do
/// for the JS path, but without the JS-side receiver / lock checks (the
/// caller has already proven those).
fn set_up_default_reader_internal(
    scope: &mut v8::PinScope,
    reader: v8::Local<v8::Object>,
    stream: v8::Local<v8::Object>,
) {
    // Allocate closed-promise resolver pair.
    let resolver = v8::PromiseResolver::new(scope).unwrap();
    let closed_promise = resolver.get_promise(scope);
    let resolver_g = v8::Global::new(scope, resolver);

    // Build state — `pending_stream` is None on this path; `after_install`
    // never runs for a Rust-built reader.
    let state = ReadableStreamDefaultReader::new_for_internal(resolver_g);
    let boxed = Box::new(state);
    let raw_ptr = Box::into_raw(boxed);
    let raw_addr = raw_ptr as usize;
    let ext = v8::External::new(scope, raw_ptr as *mut std::ffi::c_void);
    reader.set_internal_field(0, ext.into());
    let weak = v8::Weak::with_guaranteed_finalizer(
        scope,
        reader,
        Box::new(move || unsafe {
            drop(Box::from_raw(raw_addr as *mut ReadableStreamDefaultReader));
        }),
    );
    std::mem::forget(weak);

    // Run ReadableStreamReaderGenericInitialize.
    readable_stream_reader_generic_initialize(scope, reader, stream, closed_promise);
}

// ---------------------------------------------------------------------------
// ReadableStreamReaderGenericInitialize — §3.9.2
// ---------------------------------------------------------------------------

/// `ReadableStreamReaderGenericInitialize(reader, stream)` — §3.9.2 + §3.3.x.
///
/// 1. Set reader.[[stream]] = stream.
/// 2. Set stream.[[reader]] = reader.
/// 3. If stream.[[state]] is "readable":
///    a. Set reader.[[closedPromise]] to a new pending Promise.
/// 4. Else if stream.[[state]] is "closed":
///    a. Set reader.[[closedPromise]] to a Promise resolved with undefined.
/// 5. Else (errored):
///    a. Set reader.[[closedPromise]] to a Promise rejected with stream.[[storedError]].
///       (and PromiseIsHandled = true).
fn readable_stream_reader_generic_initialize<'s>(
    scope: &mut v8::PinScope<'s, '_>,
    reader: v8::Local<v8::Object>,
    stream: v8::Local<v8::Object>,
    closed_promise: v8::Local<'s, v8::Promise>,
) {
    slots::write_slot(scope, reader, STREAM, stream.into());
    slots::write_slot(scope, stream, READER, reader.into());
    slots::write_slot(scope, reader, CLOSED_PROMISE, closed_promise.into());

    // Pre-resolve / pre-reject based on the stream's current state.
    let st = match crate::streams::readable::with_rs_state(scope, stream, |s| s.state.get()) {
        Some(s) => s,
        None => return,
    };
    match st {
        StreamState::Readable => {
            // Pending — nothing to do.
        }
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
// Closed-promise helpers
// ---------------------------------------------------------------------------

/// Resolve the reader's closedPromise with undefined. Idempotent —
/// subsequent calls are no-ops because the resolver is taken (`.take()`)
/// after first use.
pub fn resolve_closed_promise(scope: &mut v8::PinScope, reader: v8::Local<v8::Object>) {
    let resolver_g = with_state(scope, reader, |s| s.closed_resolver.borrow_mut().take()).flatten();
    if let Some(resolver_g) = resolver_g {
        let resolver = v8::Local::new(scope, &resolver_g);
        let undef = v8::undefined(scope);
        resolver.resolve(scope, undef.into());
    }
}

/// Reject the reader's closedPromise with the given error. Marks the
/// promise as handled to suppress unhandled-rejection diagnostics
/// (per spec; the user observes the rejection via the closed getter).
pub fn reject_closed_promise<'s>(
    scope: &mut v8::PinScope<'s, '_>,
    reader: v8::Local<v8::Object>,
    error: v8::Local<'s, v8::Value>,
) {
    let resolver_g = with_state(scope, reader, |s| s.closed_resolver.borrow_mut().take()).flatten();
    if let Some(resolver_g) = resolver_g {
        let resolver = v8::Local::new(scope, &resolver_g);
        resolver.reject(scope, error);
        // Mark promise as handled. Spec §3.4.4 step says
        // "Set reader.[[closedPromise]].[[PromiseIsHandled]] to true".
        let cp_v = slots::read_slot(scope, reader, CLOSED_PROMISE);
        if let Ok(cp) = v8::Local::<v8::Promise>::try_from(cp_v) {
            crate::streams::promise_resolve::set_promise_is_handled_to_true(scope, cp);
        }
    }
}

// ---------------------------------------------------------------------------
// IDL methods — closed / read / releaseLock / cancel
// ---------------------------------------------------------------------------

fn closed_getter_callback(
    scope: &mut v8::PinScope,
    args: v8::FunctionCallbackArguments,
    mut rv: v8::ReturnValue,
) {
    let this = args.this();
    if !is_default_reader(scope, this) {
        // Reject Promise if receiver invalid.
        let msg = v8::String::new(scope, "closed: receiver not a reader").unwrap();
        let exc = v8::Exception::type_error(scope, msg);
        let resolver = v8::PromiseResolver::new(scope).unwrap();
        let p = resolver.get_promise(scope);
        resolver.reject(scope, exc);
        rv.set(p.into());
        return;
    }
    let cp_v = slots::read_slot(scope, this, CLOSED_PROMISE);
    rv.set(cp_v);
}

fn read_method_callback(
    scope: &mut v8::PinScope,
    args: v8::FunctionCallbackArguments,
    mut rv: v8::ReturnValue,
) {
    let this = args.this();
    if !is_default_reader(scope, this) {
        let msg = v8::String::new(scope, "read: receiver not a reader").unwrap();
        let exc = v8::Exception::type_error(scope, msg);
        let resolver = v8::PromiseResolver::new(scope).unwrap();
        let p = resolver.get_promise(scope);
        resolver.reject(scope, exc);
        rv.set(p.into());
        return;
    }
    // If the reader is detached (`[[stream]]` undefined), reject TypeError.
    let stream_v = slots::read_slot(scope, this, STREAM);
    let Ok(stream) = v8::Local::<v8::Object>::try_from(stream_v) else {
        let msg = v8::String::new(scope, "read: reader has no associated stream").unwrap();
        let exc = v8::Exception::type_error(scope, msg);
        let resolver = v8::PromiseResolver::new(scope).unwrap();
        let p = resolver.get_promise(scope);
        resolver.reject(scope, exc);
        rv.set(p.into());
        return;
    };

    // Allocate a Promise resolver for the result.
    let resolver = v8::PromiseResolver::new(scope).unwrap();
    let promise = resolver.get_promise(scope);
    let resolver_g = v8::Global::new(scope, resolver);

    let read_request = ReadRequest {
        kind: ReadRequestKind::Js {
            resolver: resolver_g,
        },
    };
    readable_stream_default_reader_read(scope, this, stream, read_request);
    rv.set(promise.into());
}

/// `ReadableStreamDefaultReaderRead(reader, readRequest)` — §3.9.2.
///
/// 1. Set [[disturbed]] on stream to true.
/// 2. If stream state == "closed": readRequest.closeSteps().
/// 3. Else if state == "errored": readRequest.errorSteps(storedError).
/// 4. Else: pull_steps(stream, readRequest).
pub fn readable_stream_default_reader_read(
    scope: &mut v8::PinScope,
    _reader: v8::Local<v8::Object>,
    stream: v8::Local<v8::Object>,
    request: ReadRequest,
) {
    crate::streams::readable::with_rs_state(scope, stream, |s| s.disturbed.set(true));
    let st = match crate::streams::readable::with_rs_state(scope, stream, |s| s.state.get()) {
        Some(s) => s,
        None => return,
    };
    match st {
        StreamState::Closed => {
            // closeSteps() — fulfill with {value: undefined, done: true}.
            fulfill_read_request_close(scope, request);
        }
        StreamState::Errored => {
            let stored = slots::read_slot(scope, stream, STORED_ERROR);
            fulfill_read_request_error(scope, request, stored);
        }
        StreamState::Readable => {
            // Dispatch on the controller's class. Byte controllers have
            // their own pull_steps that handles auto-allocate-chunk-size
            // and queue-fill paths.
            let controller_v = slots::read_slot(scope, stream, crate::streams::slots::CONTROLLER);
            if let Ok(controller) = v8::Local::<v8::Object>::try_from(controller_v) {
                if crate::streams::readable_byte_controller::is_byte_controller(scope, controller) {
                    crate::streams::readable_byte_controller::pull_steps(scope, stream, request);
                    return;
                }
            }
            crate::streams::readable_default_controller::pull_steps(scope, stream, request);
        }
    }
}

/// Push a read request onto the reader's queue (called from the
/// controller's PullSteps when the queue is empty).
pub fn enqueue_read_request(
    scope: &mut v8::PinScope,
    stream: v8::Local<v8::Object>,
    request: ReadRequest,
) {
    let reader_v = slots::read_slot(scope, stream, READER);
    let Ok(reader) = v8::Local::<v8::Object>::try_from(reader_v) else {
        return;
    };
    with_state(scope, reader, |s| s.read_requests.borrow_mut().push_back(request));
}

/// Pop the front read request from the (default) reader on `stream`.
/// Returns `None` if no default reader or no requests pending. Used by
/// the byte controller's `ProcessReadRequestsUsingQueue`.
pub fn pop_front_read_request(
    scope: &mut v8::PinScope,
    stream: v8::Local<v8::Object>,
) -> Option<ReadRequest> {
    let reader_v = slots::read_slot(scope, stream, READER);
    let reader = v8::Local::<v8::Object>::try_from(reader_v).ok()?;
    if !is_default_reader(scope, reader) {
        return None;
    }
    with_state(scope, reader, |s| s.read_requests.borrow_mut().pop_front()).flatten()
}

/// True iff `stream` has an attached default reader. Convenience for
/// debug_assert sites in the byte controller.
pub fn is_default_reader_attached(
    scope: &mut v8::PinScope,
    stream: v8::Local<v8::Object>,
) -> bool {
    let reader_v = slots::read_slot(scope, stream, READER);
    let Ok(reader) = v8::Local::<v8::Object>::try_from(reader_v) else {
        return false;
    };
    is_default_reader(scope, reader)
}

/// Fulfill a single read request with `{value: chunk, done: false}`.
/// Used by `pull_steps` when the queue had a chunk.
pub fn fulfill_read_request_chunk<'s>(
    scope: &mut v8::PinScope<'s, '_>,
    request: ReadRequest,
    chunk: v8::Local<'s, v8::Value>,
) {
    match request.kind {
        ReadRequestKind::Js { resolver } => {
            let resolver_l = v8::Local::new(scope, &resolver);
            let result = v8::Object::new(scope);
            let value_key = v8::String::new(scope, "value").unwrap();
            let done_key = v8::String::new(scope, "done").unwrap();
            result.set(scope, value_key.into(), chunk);
            result.set(scope, done_key.into(), v8::Boolean::new(scope, false).into());
            resolver_l.resolve(scope, result.into());
        }
        ReadRequestKind::Native(req) => {
            req.chunk_steps(scope, chunk);
        }
    }
}

fn fulfill_read_request_close(scope: &mut v8::PinScope, request: ReadRequest) {
    match request.kind {
        ReadRequestKind::Js { resolver } => {
            let resolver_l = v8::Local::new(scope, &resolver);
            let result = v8::Object::new(scope);
            let value_key = v8::String::new(scope, "value").unwrap();
            let done_key = v8::String::new(scope, "done").unwrap();
            result.set(scope, value_key.into(), v8::undefined(scope).into());
            result.set(scope, done_key.into(), v8::Boolean::new(scope, true).into());
            resolver_l.resolve(scope, result.into());
        }
        ReadRequestKind::Native(req) => {
            req.close_steps(scope);
        }
    }
}

fn fulfill_read_request_error<'s>(
    scope: &mut v8::PinScope<'s, '_>,
    request: ReadRequest,
    error: v8::Local<'s, v8::Value>,
) {
    match request.kind {
        ReadRequestKind::Js { resolver } => {
            let resolver_l = v8::Local::new(scope, &resolver);
            resolver_l.reject(scope, error);
        }
        ReadRequestKind::Native(req) => {
            req.error_steps(scope, error);
        }
    }
}

fn release_lock_method_callback(
    scope: &mut v8::PinScope,
    args: v8::FunctionCallbackArguments,
    _rv: v8::ReturnValue,
) {
    let this = args.this();
    if !is_default_reader(scope, this) {
        let msg = v8::String::new(scope, "releaseLock: receiver not a reader").unwrap();
        let exc = v8::Exception::type_error(scope, msg);
        scope.throw_exception(exc);
        return;
    }
    readable_stream_default_reader_release(scope, this);
}

/// `ReadableStreamDefaultReaderRelease(reader)` — §3.9.2.
///
/// 1. If reader.[[stream]] is undefined, return.
/// 2. Run ReadableStreamReaderGenericRelease(reader).
/// 3. Run ReadableStreamDefaultReaderErrorReadRequests(reader, ...) where
///    the error is a TypeError("Reader released — read requests rejected").
pub fn readable_stream_default_reader_release(
    scope: &mut v8::PinScope,
    reader: v8::Local<v8::Object>,
) {
    let stream_v = slots::read_slot(scope, reader, STREAM);
    if stream_v.is_undefined() {
        return;
    }
    readable_stream_reader_generic_release(scope, reader);
    let msg = v8::String::new(scope, "Reader released; outstanding read() requests rejected").unwrap();
    let exc = v8::Exception::type_error(scope, msg);
    let exc_l = exc.into();
    readable_stream_default_reader_error_read_requests(scope, reader, exc_l);
}

/// `ReadableStreamReaderGenericRelease(reader)` — §3.9.2.
///
/// 1. Assert reader.[[stream]] is not undefined.
/// 2. Run controller's [[ReleaseSteps]]() — for byte controllers this
///    marks the front pendingPullInto's readerType="none".
/// 3. If state == "readable": reject closedPromise with TypeError.
///    Else: replace closedPromise with a rejected one.
///    (Both: PromiseIsHandled = true).
/// 4. Set stream.[[reader]] = undefined.
/// 5. Set reader.[[stream]] = undefined.
pub fn readable_stream_reader_generic_release(
    scope: &mut v8::PinScope,
    reader: v8::Local<v8::Object>,
) {
    let stream_v = slots::read_slot(scope, reader, STREAM);
    let Ok(stream) = v8::Local::<v8::Object>::try_from(stream_v) else {
        return;
    };

    // Run controller's [[ReleaseSteps]]. For byte controllers this
    // marks the front pendingPullInto.readerType="none" so the
    // descriptor — possibly auto-allocated — survives the release.
    let controller_v = slots::read_slot(scope, stream, CONTROLLER);
    if let Ok(controller) = v8::Local::<v8::Object>::try_from(controller_v) {
        if crate::streams::readable_byte_controller::is_byte_controller(scope, controller) {
            crate::streams::readable_byte_controller::release_steps(scope, stream);
        }
    }

    let st = crate::streams::readable::with_rs_state(scope, stream, |s| s.state.get());
    let msg = v8::String::new(scope, "Reader was released and can no longer be used to monitor the stream's state").unwrap();
    let exc = v8::Exception::type_error(scope, msg);
    match st {
        Some(StreamState::Readable) => {
            // Reject the EXISTING closedPromise — its resolver is still
            // alive in the reader's state.
            reject_closed_promise(scope, reader, exc);
        }
        _ => {
            // Replace closedPromise with a fresh, already-rejected one.
            let resolver = v8::PromiseResolver::new(scope).unwrap();
            resolver.reject(scope, exc);
            let p = resolver.get_promise(scope);
            slots::write_slot(scope, reader, CLOSED_PROMISE, p.into());
            crate::streams::promise_resolve::set_promise_is_handled_to_true(scope, p);
        }
    }

    // Detach reader ↔ stream.
    slots::delete_slot(scope, stream, READER);
    slots::delete_slot(scope, reader, STREAM);
}

/// `ReadableStreamDefaultReaderErrorReadRequests(reader, e)` — §3.9.2.
pub fn readable_stream_default_reader_error_read_requests<'s>(
    scope: &mut v8::PinScope<'s, '_>,
    reader: v8::Local<v8::Object>,
    error: v8::Local<'s, v8::Value>,
) {
    let drained: Vec<ReadRequest> = with_state(scope, reader, |state| {
        state.read_requests.borrow_mut().drain(..).collect()
    })
    .unwrap_or_default();
    for req in drained {
        fulfill_read_request_error(scope, req, error);
    }
}

fn cancel_method_callback<'s>(
    scope: &mut v8::PinScope<'s, '_>,
    args: v8::FunctionCallbackArguments<'s>,
    mut rv: v8::ReturnValue<'s>,
) {
    let this = args.this();
    if !is_default_reader(scope, this) {
        let msg = v8::String::new(scope, "cancel: receiver not a reader").unwrap();
        let exc = v8::Exception::type_error(scope, msg);
        let resolver = v8::PromiseResolver::new(scope).unwrap();
        let p = resolver.get_promise(scope);
        resolver.reject(scope, exc);
        rv.set(p.into());
        return;
    }
    let stream_v = slots::read_slot(scope, this, STREAM);
    let Ok(stream) = v8::Local::<v8::Object>::try_from(stream_v) else {
        // Spec §3.3.4.2: reject with TypeError if [[stream]] undefined.
        let msg = v8::String::new(scope, "cancel: reader has no associated stream").unwrap();
        let exc = v8::Exception::type_error(scope, msg);
        let resolver = v8::PromiseResolver::new(scope).unwrap();
        let p = resolver.get_promise(scope);
        resolver.reject(scope, exc);
        rv.set(p.into());
        return;
    };
    let reason = args.get(0);
    let promise = readable_stream_reader_generic_cancel(scope, this, stream, reason);
    rv.set(promise.into());
}

/// `ReadableStreamReaderGenericCancel(reader, reason)` — §3.9.2.
/// Returns ReadableStreamCancel(reader.[[stream]], reason).
pub fn readable_stream_reader_generic_cancel<'s>(
    scope: &mut v8::PinScope<'s, '_>,
    _reader: v8::Local<v8::Object>,
    stream: v8::Local<v8::Object>,
    reason: v8::Local<'s, v8::Value>,
) -> v8::Local<'s, v8::Promise> {
    algorithms::readable_stream_cancel(scope, stream, reason)
}

// ---------------------------------------------------------------------------
// Public install
// ---------------------------------------------------------------------------

pub fn install(scope: &mut v8::PinScope, global: v8::Local<v8::Object>) {
    // Macro-emitted FunctionTemplate carries the constructor + Symbol.toStringTag.
    let tmpl = ReadableStreamDefaultReader::install(scope);

    // Patch in the IDL methods (read / releaseLock / cancel) and the
    // closed getter on the prototype. They stay raw FunctionCallbacks
    // because they need `args.this()` access (priv-sym reads) and direct
    // PromiseResolver allocation; the macro's `#[v8_method]` shape would
    // require widening every body to `&self`-plus-synthetic-`this` and
    // re-routing through `with_state`, which is out of scope for this
    // constructor-only migration.
    //
    // The headers.rs `install_global` pattern (lines 895–940) does exactly
    // this for keys/values/entries/forEach.
    let class_fn = tmpl.get_function(scope).unwrap();
    let proto_key = v8::String::new(scope, "prototype").unwrap();
    let proto_v = class_fn.get(scope, proto_key.into()).unwrap();
    let proto: v8::Local<v8::Object> = proto_v.try_into().unwrap();

    // closed getter (mixin §3.3) — attach via PropertyDescriptor since
    // we're operating on the realised prototype Object, not an
    // ObjectTemplate (which would expose `set_accessor_property`).
    // No-setter shape via `new_from_get_set` with `undefined` setter,
    // matching the body consumer accessor pattern in
    // `fetch/body/consumers.rs:110`.
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

    let key = v8::String::new(scope, "ReadableStreamDefaultReader").unwrap();
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
