//! Native consumption of JS ReadableStreams through native byte channels.
//!
//! The runtime retains a reader, converts its results to bytes and delivers
//! them through the attached writer, a bounded channel a native consumer
//! drains: response bodies to the kernel, RPC stream procedures,
//! `env.storage.putStream` and `fetch` request bodies. (Body consumers such
//! as `Response.text()` read through `fetch::body::body_stream`, and
//! `EventSource` drives its response reader itself.) Uploads and RPC
//! iterators pause while the consumer's buffer is full; an upload also
//! slices a chunk larger than the buffer's free space, so no single chunk is
//! too large to send. A response body or RPC iterator is read through the
//! public reader methods, so creator-supplied stream implementations can
//! participate; an upload reads its own native stream through the internal
//! reader algorithms.
//!
//! Promise callbacks identify the live forwarder without retaining its reader.
//! RPC cancellation and deadlines follow the response body after headers,
//! and source cancellation runs in the captured request context.

use std::cell::RefCell;
use std::collections::VecDeque;
use std::fmt;
use std::rc::{Rc, Weak};
use std::task::{Context, Poll};
use std::time::Instant;

use compio::bytes::Bytes;

use crate::rpc::lifetime::{Cancellation, RequestLifetime};

use crate::channel::{stream_buffer_in_share, StreamPushResult, StreamReader, StreamShare, StreamWriter};
use crate::state::SharedState;

// ---------------------------------------------------------------------------
// Forwarder state
// ---------------------------------------------------------------------------

/// Shared state between the Rust promise-reaction callbacks and the
/// kernel's `attach_writer` / `is_closed` queries. Lives in a
/// `Rc<RefCell<>>` so the JS callbacks (which V8 keeps alive via the
/// pending `read()` Promise's reaction list) can mutate it without
/// crossing thread boundaries.
#[derive(Default)]
pub struct StreamForwarderInner {
    /// Bytes not yet handed to `direct_writer`: every chunk read before a
    /// writer is attached, and, for an upload, the part of a chunk that did
    /// not fit in the writer's free space. An upload does not read again
    /// until this is empty.
    pub buffer: VecDeque<Vec<u8>>,
    /// True once `read()` returned `{done: true}` OR a read rejected.
    pub closed: bool,
    /// Kernel-attached writer. When `None`, chunks queue in `buffer`.
    /// When `Some`, `buffer` MUST be empty (the kernel drains it on
    /// attach) and incoming chunks push straight to the writer's
    /// channel.
    pub direct_writer: Option<StreamWriter>,
    /// The locked `reader` (from `getReader()`). Persisted so a paused
    /// read loop can be re-armed by `resume_read` after the consumer drains
    /// the downstream buffer, or cancelled when the response consumer leaves.
    pub reader: Option<v8::Global<v8::Object>>,
    /// True while the read loop is suspended for backpressure: the writer
    /// buffer crossed the high-water mark, so we stopped re-arming
    /// `reader.read()`. `resume_read` flips this back and schedules the next
    /// read once the consumer has drained below the low-water mark.
    pub paused: bool,
    /// Continuation context captured when the app supplied the stream.
    pub continuation_context: Option<v8::Global<v8::Value>>,
    /// Pause the producer until its consumer drains the channel. Enabled for
    /// uploads and native RPC iterators. Ordinary Response bodies retain their
    /// existing read scheduling.
    pub backpressure: bool,
    cancel_requested: Option<Cancellation>,
    request: Option<RequestLifetime>,
    rpc_framing: bool,
    error: Option<String>,
    /// An upload hands the writer only what fits and keeps the rest in
    /// `buffer`, so a chunk of any size is accepted within the byte cap.
    slices: bool,
    /// How a `read()` result's value becomes bytes.
    chunk_policy: ChunkPolicy,
    /// The `fetch` signal an upload follows. Once it has aborted, its reason
    /// is what the source is cancelled with.
    abort_signal: Option<v8::Global<v8::Object>>,
    /// The runtime's upload share an upload's channel and parked bytes
    /// count against.
    share: Option<Rc<StreamShare>>,
    /// Bytes in `buffer` charged to `share`.
    parked: usize,
    /// The request that started an upload. When that request ends early
    /// (timeout or cancellation), the upload ends with it.
    owner_request: Option<u64>,
    /// An upload reads its own native stream through the internal reader
    /// algorithms, so creator code cannot substitute the reader.
    internal_reader: bool,
}

impl Drop for StreamForwarderInner {
    fn drop(&mut self) {
        if let Some(share) = &self.share {
            share.release(self.parked);
        }
    }
}

impl StreamForwarderInner {
    /// Queue `data` behind `buffer`, charging an upload's share.
    fn park_back(&mut self, data: Vec<u8>) {
        self.charge_parked(data.len());
        self.buffer.push_back(data);
    }

    /// Queue `data` at the front of `buffer`, charging an upload's share.
    fn park_front(&mut self, data: Vec<u8>) {
        self.charge_parked(data.len());
        self.buffer.push_front(data);
    }

    /// Take the front of `buffer`, releasing its charge.
    fn unpark_front(&mut self) -> Option<Vec<u8>> {
        let data = self.buffer.pop_front()?;
        if self.share.is_some() {
            self.parked = self.parked.saturating_sub(data.len());
            if let Some(share) = &self.share {
                share.release(data.len());
            }
        }
        Some(data)
    }

    fn charge_parked(&mut self, n: usize) {
        if let Some(share) = &self.share {
            share.charge(n);
            self.parked += n;
        }
    }
}

/// How an upload is read and where its bytes are accounted.
pub struct UploadOptions<'s> {
    /// Bytes the upload's channel holds before the producer pauses.
    pub buffer_cap: usize,
    pub chunk_policy: ChunkPolicy,
    /// The `fetch` signal of a request body; its abort cancels the source
    /// with its reason.
    pub abort_signal: Option<v8::Local<'s, v8::Object>>,
    /// The request that started the upload; see [`cancel_uploads_owned_by`].
    pub owner_request: Option<u64>,
}

/// How the forwarder turns the `value` of a `read()` result into bytes.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum ChunkPolicy {
    /// Bytes of any ArrayBufferView or ArrayBuffer; UTF-8 of anything else.
    #[default]
    Coerce,
    /// Fetch "incrementally read": a chunk that is not a Uint8Array errors
    /// the body with a TypeError and cancels the source with it.
    Uint8Array,
}

/// Resume the read loop on a forwarder that paused for backpressure. Called
/// by the pump (which holds a V8 scope) after the consumer enqueues this
/// `stream_id` in `RuntimeState::forwarder_resumes`. No-op if the forwarder
/// is gone, closed, not paused, or has no persisted reader.
pub fn resume_read(scope: &mut v8::PinScope, state: &SharedState, stream_id: u32) {
    let Some(fwd) = get(state, stream_id) else { return };
    if fwd.borrow().cancel_requested.is_some() {
        cancel_reader(scope, state, stream_id, &fwd);
        return;
    }
    let flushed = {
        let mut inner = fwd.borrow_mut();
        if inner.closed || !inner.paused {
            return;
        }
        flush_parked(&mut inner)
    };
    if let Some(reason) = refusal(flushed) {
        request_cancel(state, stream_id, reason);
        return;
    }
    // Parked bytes that still do not fit, or a writer still above the
    // watermark, keep the producer paused until the consumer drains more.
    if should_pause(&fwd) {
        return;
    }
    let (reader, continuation_context) = {
        let mut inner = fwd.borrow_mut();
        let Some(reader) = inner.reader.clone() else { return };
        inner.paused = false;
        (reader, inner.continuation_context.clone())
    };
    match continuation_context {
        Some(context) => crate::core::invocation::with_captured_context(
            scope,
            &context,
            |scope| schedule_next_read(scope, reader, fwd, stream_id, state.clone()),
        ),
        None => schedule_next_read(scope, reader, fwd, stream_id, state.clone()),
    }
}

pub type StreamForwarder = Rc<RefCell<StreamForwarderInner>>;

// ---------------------------------------------------------------------------
// Registry: stream_id -> StreamForwarder
// ---------------------------------------------------------------------------

/// Insert a forwarder into the per-isolate map. Caller must hold the
/// SharedState mutex (we do that internally).
fn register(state: &SharedState, stream_id: u32, fwd: StreamForwarder) {
    state.borrow_mut().stream_forwarders.insert(stream_id, fwd);
}

/// Look up a forwarder by stream_id.
pub fn get(state: &SharedState, stream_id: u32) -> Option<StreamForwarder> {
    state.borrow().stream_forwarders.get(&stream_id).cloned()
}

/// Transfer cancellation ownership from RPC invocation to its response body.
pub(crate) fn retain_request(
    scope: &mut v8::PinScope,
    state: &SharedState,
    stream_id: u32,
    request: RequestLifetime,
) {
    let Some(fwd) = get(state, stream_id) else { return; };
    if fwd.borrow().closed { return; }
    let signal = request.signal.local(scope);
    fwd.borrow_mut().request = Some(request);
    let weak_state = Rc::downgrade(state);
    crate::dom::abort_signal::add_abort_algorithm(scope, signal, Box::new(move || {
        if let Some(state) = weak_state.upgrade() {
            request_cancel(&state, stream_id, Cancellation::Cancelled);
        }
    }));
    if crate::dom::abort_signal::is_aborted(scope, signal) {
        request_cancel(state, stream_id, Cancellation::Cancelled);
    }
    state.borrow().notify_pump();
}

pub(crate) fn owns_request(state: &SharedState, stream_id: u32) -> bool {
    get(state, stream_id).is_some_and(|fwd| fwd.borrow().request.is_some())
}

pub(crate) fn next_deadline(state: &SharedState) -> Option<Instant> {
    state.borrow().stream_forwarders.values().filter_map(|fwd| {
        let inner = fwd.borrow();
        if inner.closed || inner.cancel_requested.is_some() { return None; }
        inner.request.as_ref().and_then(|request| request.deadline)
    }).min()
}

pub(crate) fn poll_cancellation(state: &SharedState, cx: &mut std::task::Context<'_>) -> bool {
    state.borrow().stream_forwarders.values().any(|fwd| {
        let inner = fwd.borrow();
        if inner.closed || inner.cancel_requested.is_some() { return false; }
        inner.request.as_ref().is_some_and(|request| {
            request.cancel.register_waker(cx.waker());
            request.cancel.is_cancelled()
        })
    })
}

pub(crate) fn queue_cancellations(state: &SharedState, now: Instant) {
    let cancelled: Vec<_> = state.borrow().stream_forwarders.iter().filter_map(|(&id, fwd)| {
        let inner = fwd.borrow();
        if inner.closed || inner.cancel_requested.is_some() { return None; }
        inner.request.as_ref()?.cancellation(now).map(|reason| (id, reason))
    }).collect();
    for (id, reason) in cancelled { request_cancel(state, id, reason); }
}

/// Remove a forwarder from the registry. Called by the kernel after
/// the writer has been attached + the buffered chunks drained, or
/// after the stream errors.
pub fn remove(state: &SharedState, stream_id: u32) {
    state.borrow_mut().stream_forwarders.remove(&stream_id);
}

// ---------------------------------------------------------------------------
// begin_forward — entry from `transport::handler::inspect_response`
// ---------------------------------------------------------------------------

/// Lock the Response body's ReadableStream and start pumping into
/// a fresh forwarder. Returns the allocated `stream_id` so the
/// kernel can match this forward to the eventual writer attachment.
///
/// Errors are surfaced as `Err(message)` strings (mirrors
/// `inspect_response`'s `Result<_, String>` shape). On success the
/// forwarder is registered + the read loop has scheduled its first
/// reaction; subsequent chunks flow async via promise resolution.
pub fn begin_forward(
    scope: &mut v8::PinScope,
    response_obj: v8::Local<v8::Object>,
) -> Result<u32, String> {
    // Idempotence: if `_streamId` is already stamped, return it. This
    // matches the JS shim's behaviour — `inspect_response` may run twice
    // on the same Response during cancel/replay paths.
    let id_key = v8::String::new(scope, "_streamId").unwrap();
    if let Some(existing) = response_obj.get(scope, id_key.into())
        && !existing.is_undefined()
        && let Some(id) = existing.uint32_value(scope)
        && id != u32::MAX
    {
        return Ok(id);
    }

    // Read response.body.
    let body_key = v8::String::new(scope, "body").unwrap();
    let body_v = response_obj
        .get(scope, body_key.into())
        .ok_or_else(|| "begin_forward: response.body access threw".to_string())?;
    let body_obj = v8::Local::<v8::Object>::try_from(body_v)
        .map_err(|_| "begin_forward: response.body is not an object".to_string())?;

    // Wire response-body path: NO upload backpressure (the TCP consumer does
    // not re-arm a paused producer). Keeps the original eager read loop.
    let state = runtime_state(scope)?;
    let stream_id =
        forward_from_readable(scope, &state, body_obj, ForwardMode::Body, ChunkPolicy::Coerce, None)?;

    // Stamp the id on the response so the kernel can read it later
    // for idempotent re-inspect — and so build_fetch_outcome can match
    // this forwarder to the writer attach.
    let id_v = v8::Integer::new_from_unsigned(scope, stream_id);
    response_obj.set(scope, id_key.into(), id_v.into());
    Ok(stream_id)
}

/// Read a **bare `ReadableStream`** into a native upload channel.
///
/// Same pump as [`begin_forward`] (lock via `getReader()`, drive
/// `reader.read()` in a Rust promise-reaction loop), with upload
/// backpressure: the read loop pauses while the channel is half full and
/// resumes once the returned [`UploadReader`] drains it to a quarter, and a
/// chunk larger than the free space is sliced. The channel and the parked
/// part of a chunk count against the runtime's upload share, so an app's
/// uploads together stay within it.
///
/// The stream is locked and read through the internal reader algorithms:
/// a creator cannot substitute `getReader`, `read` or `cancel`.
///
/// Used by `env.storage.putStream` (`Backend::put_stream`) and by `fetch`
/// for a `ReadableStream` request body.
///
/// # Errors
///
/// Outside a runtime isolate, when `stream_obj` is not a native
/// ReadableStream, or when it is locked; each is checked before the stream
/// is touched.
pub fn forward_upload(
    scope: &mut v8::PinScope,
    stream_obj: v8::Local<v8::Object>,
    options: UploadOptions<'_>,
) -> Result<UploadReader, String> {
    let state = runtime_state(scope)?;
    if !crate::streams::readable::is_readable_stream(scope, stream_obj) {
        return Err("the upload source is not a ReadableStream".to_string());
    }
    let reader = crate::streams::readable_default_reader::acquire_readable_stream_default_reader(
        scope, stream_obj,
    )?;
    let share = state.borrow().upload_share.clone();
    let stream_id = start_forwarder(
        scope,
        &state,
        reader,
        ForwardMode::Upload,
        options.chunk_policy,
        options.abort_signal,
    )?;
    if let Some(fwd) = get(&state, stream_id) {
        let mut inner = fwd.borrow_mut();
        inner.share = Some(share.clone());
        inner.owner_request = options.owner_request;
    }
    let (writer, reader) = stream_buffer_in_share(options.buffer_cap, share);
    attach_writer(&state, stream_id, writer);
    Ok(UploadReader { reader, state: Rc::downgrade(&state), stream_id })
}

/// Shared core: lock `readable` via `getReader()`, register a forwarder,
/// and schedule the first read. Returns the new `stream_id`.
fn forward_from_readable(
    scope: &mut v8::PinScope,
    state: &SharedState,
    body_obj: v8::Local<v8::Object>,
    mode: ForwardMode,
    chunk_policy: ChunkPolicy,
    abort_signal: Option<v8::Local<v8::Object>>,
) -> Result<u32, String> {
    // Lock via getReader().
    let get_reader_key = v8::String::new(scope, "getReader").unwrap();
    let get_reader_v = body_obj
        .get(scope, get_reader_key.into())
        .ok_or_else(|| "begin_forward: body.getReader access threw".to_string())?;
    let get_reader_fn = v8::Local::<v8::Function>::try_from(get_reader_v)
        .map_err(|_| "begin_forward: body.getReader is not a function".to_string())?;

    // tc_scope to capture exceptions thrown by getReader (e.g. stream
    // already locked) so we surface them as Err(message) instead of
    // letting them propagate up the V8 callback stack.
    let reader_global: v8::Global<v8::Object> = {
        v8::tc_scope!(let tc, scope);
        let result = get_reader_fn.call(tc, body_obj.into(), &[]);
        let Some(reader_v) = result else {
            let msg = tc
                .exception()
                .and_then(|e| e.to_string(tc))
                .map(|s| s.to_rust_string_lossy(tc))
                .unwrap_or_else(|| "getReader threw".to_string());
            return Err(msg);
        };
        let reader_obj = v8::Local::<v8::Object>::try_from(reader_v)
            .map_err(|_| "begin_forward: getReader did not return an object".to_string())?;
        v8::Global::new(tc, reader_obj)
    };

    let reader = v8::Local::new(scope, reader_global);
    start_forwarder(scope, state, reader, mode, chunk_policy, abort_signal)
}

/// The isolate's runtime state. A forwarder registers there, so stream
/// forwarding outside a runtime isolate is refused before any stream is
/// touched, rather than panicking inside a V8 callback.
fn runtime_state(scope: &mut v8::PinScope) -> Result<SharedState, String> {
    scope
        .get_slot::<SharedState>()
        .cloned()
        .ok_or_else(|| "stream forwarding runs only inside a runtime isolate".to_string())
}

#[derive(Clone, Copy, PartialEq, Eq)]
pub(crate) enum ForwardMode { Body, Upload, Rpc }

/// Forward a host-supplied reader through the same body channel as a Response.
pub(crate) fn begin_forward_reader(
    scope: &mut v8::PinScope,
    reader: v8::Local<v8::Object>,
    mode: ForwardMode,
) -> Result<u32, String> {
    let state = runtime_state(scope)?;
    start_forwarder(scope, &state, reader, mode, ChunkPolicy::Coerce, None)
}

fn start_forwarder(
    scope: &mut v8::PinScope,
    state: &SharedState,
    reader: v8::Local<v8::Object>,
    mode: ForwardMode,
    chunk_policy: ChunkPolicy,
    abort_signal: Option<v8::Local<v8::Object>>,
) -> Result<u32, String> {
    let reader_global = v8::Global::new(scope, reader);
    // Allocate stream_id.
    let state = state.clone();
    let stream_id = state.borrow_mut().alloc_stream_id();

    // Allocate forwarder and register. Persist the reader so a backpressure
    // pause can be resumed later by `resume_read`.
    let fwd: StreamForwarder = Rc::new(RefCell::new(StreamForwarderInner::default()));
    let continuation_context = crate::core::invocation::capture_context(scope);
    {
        let mut inner = fwd.borrow_mut();
        inner.reader = Some(reader_global.clone());
        inner.continuation_context = Some(continuation_context);
        inner.backpressure = mode != ForwardMode::Body;
        inner.rpc_framing = mode == ForwardMode::Rpc;
        inner.slices = mode == ForwardMode::Upload;
        inner.internal_reader = mode == ForwardMode::Upload;
        inner.chunk_policy = chunk_policy;
        inner.abort_signal = abort_signal.map(|signal| v8::Global::new(scope, signal));
    }
    register(&state, stream_id, fwd.clone());

    if let Some(signal) = abort_signal {
        let weak_state = Rc::downgrade(&state);
        crate::dom::abort_signal::add_abort_algorithm(scope, signal, Box::new(move || {
            if let Some(state) = weak_state.upgrade() {
                request_cancel(&state, stream_id, Cancellation::Cancelled);
            }
        }));
        if crate::dom::abort_signal::is_aborted(scope, signal) {
            request_cancel(&state, stream_id, Cancellation::Cancelled);
            return Ok(stream_id);
        }
    }

    // Schedule the first read.
    schedule_next_read(scope, reader_global, fwd, stream_id, state);

    Ok(stream_id)
}

// ---------------------------------------------------------------------------
// schedule_next_read — drive `reader.read()` and attach reaction callbacks
// ---------------------------------------------------------------------------

/// Pump one chunk: call `reader.read()`, attach `.then(on_chunk, on_error)`.
/// `on_chunk` re-arms by calling this function recursively (via the
/// promise-reaction microtask queue, so no Rust-side recursion).
fn schedule_next_read(
    scope: &mut v8::PinScope,
    reader_global: v8::Global<v8::Object>,
    fwd: StreamForwarder,
    stream_id: u32,
    state: SharedState,
) {
    let reader = v8::Local::new(scope, &reader_global);
    if fwd.borrow().internal_reader {
        // ReadableStreamDefaultReaderRead with a promise read request: the
        // same `{ value, done }` result `reader.read()` resolves with, without
        // looking anything up on the reader.
        let stream_v = crate::streams::slots::read_slot(scope, reader, crate::streams::slots::STREAM);
        let (Ok(stream), Some(resolver)) =
            (v8::Local::<v8::Object>::try_from(stream_v), v8::PromiseResolver::new(scope))
        else {
            error_forwarder(&fwd, &state, stream_id, "the upload's reader has no stream");
            return;
        };
        let promise = resolver.get_promise(scope);
        let request = crate::streams::readable_default_reader::ReadRequest {
            kind: crate::streams::readable_default_reader::ReadRequestKind::Js {
                resolver: v8::Global::new(scope, resolver),
            },
        };
        crate::streams::readable_default_reader::readable_stream_default_reader_read(
            scope, reader, stream, request,
        );
        let on_chunk = make_on_chunk_callback(scope, stream_id);
        let on_error = make_on_error_callback(scope, stream_id);
        promise.then2(scope, on_chunk, on_error);
        return;
    }
    let read_key = v8::String::new(scope, "read").unwrap();
    let read_v = match reader.get(scope, read_key.into()) {
        Some(v) => v,
        None => {
            error_forwarder(&fwd, &state, stream_id, "reader.read access failed");
            return;
        }
    };
    let read_fn = match v8::Local::<v8::Function>::try_from(read_v) {
        Ok(f) => f,
        Err(_) => {
            error_forwarder(&fwd, &state, stream_id, "reader.read is not a function");
            return;
        }
    };

    // Call reader.read() in a tc_scope so a sync-throw doesn't unwind
    // V8 — it gets routed to error_forwarder instead.
    let promise_global: v8::Global<v8::Promise> = {
        v8::tc_scope!(let tc, scope);
        let promise_v = read_fn.call(tc, reader.into(), &[]);
        let Some(promise_v) = promise_v else {
            // Sync exception in read() — error and abort.
            let msg = tc
                .exception()
                .and_then(|e| e.to_string(tc))
                .map(|s| s.to_rust_string_lossy(tc))
                .unwrap_or_else(|| "read() threw".to_string());
            error_forwarder(&fwd, &state, stream_id, &msg);
            return;
        };
        let promise = match v8::Local::<v8::Promise>::try_from(promise_v) {
            Ok(p) => p,
            Err(_) => {
                error_forwarder(&fwd, &state, stream_id, "read() did not return a Promise");
                return;
            }
        };
        v8::Global::new(tc, promise)
    };

    // Promise reactions carry the stream identifier. The registry owns
    // the reader only while the forwarder is live.
    let on_chunk = make_on_chunk_callback(scope, stream_id);
    let on_error = make_on_error_callback(scope, stream_id);

    let promise = v8::Local::new(scope, &promise_global);
    promise.then2(scope, on_chunk, on_error);
}

// ---------------------------------------------------------------------------
// on_chunk / on_error — promise-reaction callbacks
// ---------------------------------------------------------------------------

// Callback data contains only a stream identifier. Looking up the live
// forwarder avoids rooting its reader through the promise reaction itself.
fn make_on_chunk_callback<'s>(scope: &mut v8::PinScope<'s, '_>, stream_id: u32) -> v8::Local<'s, v8::Function> {
    let id = v8::Integer::new_from_unsigned(scope, stream_id);
    v8::FunctionTemplate::builder(on_chunk_callback).data(id.into()).build(scope).get_function(scope).unwrap()
}

fn make_on_error_callback<'s>(scope: &mut v8::PinScope<'s, '_>, stream_id: u32) -> v8::Local<'s, v8::Function> {
    let id = v8::Integer::new_from_unsigned(scope, stream_id);
    v8::FunctionTemplate::builder(on_error_callback).data(id.into()).build(scope).get_function(scope).unwrap()
}

fn on_chunk_callback(
    scope: &mut v8::PinScope,
    args: v8::FunctionCallbackArguments,
    _rv: v8::ReturnValue,
) {
    let Some(stream_id) = args.data().uint32_value(scope) else { return; };
    let Some(state) = scope.get_slot::<SharedState>().cloned() else { return; };
    let Some(fwd) = get(&state, stream_id) else { return; };
    if fwd.borrow().closed || fwd.borrow().cancel_requested.is_some() { return; }

    // The argument is `{ value, done }` (the Promise resolution of
    // `reader.read()`).
    let result = args.get(0);
    let result_obj = match v8::Local::<v8::Object>::try_from(result) {
        Ok(o) => o,
        Err(_) => {
            error_forwarder(
                &fwd,
                &state,
                stream_id,
                "read() resolved with non-object",
            );
            return;
        }
    };

    let done_key = v8::String::new(scope, "done").unwrap();
    let done = result_obj
        .get(scope, done_key.into())
        .map(|v| v.boolean_value(scope))
        .unwrap_or(false);

    if done {
        close_forwarder(&fwd, &state, stream_id);
        return;
    }

    let value_key = v8::String::new(scope, "value").unwrap();
    let value = match result_obj.get(scope, value_key.into()) {
        Some(v) => v,
        None => {
            error_forwarder(
                &fwd,
                &state,
                stream_id,
                "read() result has no `value`",
            );
            return;
        }
    };

    if fwd.borrow().chunk_policy == ChunkPolicy::Uint8Array && !value.is_uint8_array() {
        reject_chunk(scope, &fwd, &state, stream_id, "ReadableStream chunk is not a Uint8Array");
        return;
    }

    // Normalise to bytes:
    //   - Uint8Array / typed array / DataView (ArrayBufferView): copy
    //     contents.
    //   - ArrayBuffer: wrap as Uint8Array and copy.
    //   - String: UTF-8 encode.
    //   - Anything else: stringify + UTF-8 encode (mirrors the old
    //     JS shim's coercion behaviour for compat).
    let bytes = if let Ok(view) = v8::Local::<v8::ArrayBufferView>::try_from(value) {
        let mut buf = vec![0u8; view.byte_length()];
        view.copy_contents(&mut buf);
        buf
    } else if let Ok(ab) = v8::Local::<v8::ArrayBuffer>::try_from(value) {
        let len = ab.byte_length();
        let mut buf = vec![0u8; len];
        let store = ab.get_backing_store();
        for (i, slot) in store.iter().enumerate().take(len) {
            buf[i] = slot.get();
        }
        buf
    } else if value.is_string() {
        value.to_rust_string_lossy(scope).into_bytes()
    } else {
        // Best-effort: ToString + UTF-8.
        value.to_rust_string_lossy(scope).into_bytes()
    };

    if !push_chunk(&fwd, &state, stream_id, bytes) {
        return;
    }

    // Backpressure: if the downstream buffer is now over the high-water mark,
    // PAUSE the read loop instead of re-arming. The consumer of the paired
    // StreamReader re-arms us via `resume_read` (pump-serviced) once it has
    // drained below the low-water mark. This bounds a large streaming upload
    // to the buffer cap instead of letting the read loop race ahead and
    // overflow it. A wire-path forwarder (no high-water hit, or already
    // direct-draining to the TCP channel faster than V8 produces) simply
    // never pauses.
    if should_pause(&fwd) {
        fwd.borrow_mut().paused = true;
        return;
    }

    // Re-arm using the reader still owned by the live forwarder.
    let Some(reader) = fwd.borrow().reader.clone() else { return; };
    schedule_next_read(
        scope,
        reader,
        fwd.clone(),
        stream_id,
        state.clone(),
    );
}

/// Pause before writer attachment, while bytes are parked, or when the
/// writer's buffer reaches the producer watermark. Attachment and consumer
/// draining enqueue the next pull.
fn should_pause(fwd: &StreamForwarder) -> bool {
    let inner = fwd.borrow();
    inner.backpressure
        && (!inner.buffer.is_empty()
            || inner
                .direct_writer
                .as_ref()
                .is_none_or(|w| w.buffered_bytes() >= w.cap() / 2))
}

/// Fetch's incrementally-read loop: a chunk that is not a Uint8Array is a
/// TypeError. The consumer sees the failure and the source is cancelled
/// with the TypeError, in the context that supplied the stream.
fn reject_chunk(
    scope: &mut v8::PinScope,
    fwd: &StreamForwarder,
    state: &SharedState,
    stream_id: u32,
    msg: &str,
) {
    let (reader, frame, internal) = {
        let mut inner = fwd.borrow_mut();
        (inner.reader.take(), inner.continuation_context.take(), inner.internal_reader)
    };
    error_forwarder(fwd, state, stream_id, msg);
    let Some(reader) = reader else { return };
    let cancel = |scope: &mut v8::PinScope| {
        let text = crate::web::js_text(scope, msg);
        let reason = v8::Exception::type_error(scope, text);
        cancel_source(scope, reader, reason, internal);
    };
    match frame {
        Some(frame) => crate::core::invocation::with_captured_context(scope, &frame, cancel),
        None => cancel(scope),
    }
}

/// Cancel the source with `reason`, its promise marked handled: the
/// forwarder has already reported the outcome. An upload's own native
/// reader goes through ReadableStreamReaderGenericCancel; any other reader
/// through its public `cancel` method.
fn cancel_source<'s>(
    scope: &mut v8::PinScope<'s, '_>,
    reader: v8::Global<v8::Object>,
    reason: v8::Local<'s, v8::Value>,
    internal: bool,
) {
    if internal {
        // ReadableStreamCancel settles a throwing cancel algorithm as a
        // rejected promise, so nothing escapes this call.
        let reader = v8::Local::new(scope, reader);
        let stream_v = crate::streams::slots::read_slot(scope, reader, crate::streams::slots::STREAM);
        if let Ok(stream) = v8::Local::<v8::Object>::try_from(stream_v) {
            let cancelled = crate::streams::readable_default_reader::readable_stream_reader_generic_cancel(
                scope, reader, stream, reason,
            );
            cancelled.mark_as_handled();
        }
        return;
    }
    v8::tc_scope!(let tc, scope);
    let reader = v8::Local::new(tc, reader);
    let key = v8::String::new(tc, "cancel").unwrap();
    if let Some(method) = reader.get(tc, key.into())
        && let Ok(method) = v8::Local::<v8::Function>::try_from(method)
        && let Some(result) = method.call(tc, reader.into(), &[reason])
        && let Some(resolver) = v8::PromiseResolver::new(tc)
    {
        resolver.get_promise(tc).mark_as_handled();
        resolver.resolve(tc, result);
    }
}

fn on_error_callback(
    scope: &mut v8::PinScope,
    args: v8::FunctionCallbackArguments,
    _rv: v8::ReturnValue,
) {
    let Some(stream_id) = args.data().uint32_value(scope) else { return; };
    let Some(state) = scope.get_slot::<SharedState>().cloned() else { return; };
    let Some(fwd) = get(&state, stream_id) else { return; };
    if fwd.borrow().closed || fwd.borrow().cancel_requested.is_some() { return; }

    let err = args.get(0);
    let msg = if err.is_object() {
        let err_obj: v8::Local<v8::Object> = err.try_into().unwrap();
        let msg_key = v8::String::new(scope, "message").unwrap();
        err_obj
            .get(scope, msg_key.into())
            .filter(|v| !v.is_undefined())
            .map(|v| v.to_rust_string_lossy(scope))
            .unwrap_or_else(|| err.to_rust_string_lossy(scope))
    } else {
        err.to_rust_string_lossy(scope)
    };

    error_forwarder(&fwd, &state, stream_id, &msg);
}

// ---------------------------------------------------------------------------
// Forwarder I/O — used by the callbacks above
// ---------------------------------------------------------------------------

fn push_chunk(
    fwd: &StreamForwarder,
    state: &SharedState,
    stream_id: u32,
    data: Vec<u8>,
) -> bool {
    let mut inner = fwd.borrow_mut();
    if inner.closed || inner.cancel_requested.is_some() {
        return false;
    }
    if inner.direct_writer.is_none() || !inner.buffer.is_empty() {
        inner.park_back(data);
        return true;
    }
    let result = write_or_park(&mut inner, data);
    drop(inner);
    match refusal(result) {
        None => true,
        Some(reason) => {
            request_cancel(state, stream_id, reason);
            false
        }
    }
}

/// Hand `data` to the attached writer. A slicing forwarder writes only what
/// fits in the writer's free space and parks the rest at the front of
/// `buffer`, so the writer's byte cap holds for a chunk of any size.
fn write_or_park(inner: &mut StreamForwarderInner, mut data: Vec<u8>) -> StreamPushResult {
    let Some(room) = inner.direct_writer.as_ref().map(StreamWriter::room) else {
        inner.park_front(data);
        return StreamPushResult::Ok;
    };
    if inner.slices && data.len() > room {
        let rest = data.split_off(room);
        inner.park_front(rest);
        if data.is_empty() {
            return StreamPushResult::Ok;
        }
    }
    match inner.direct_writer.as_ref() {
        Some(writer) => writer.push(data),
        None => StreamPushResult::Closed,
    }
}

/// Move parked bytes into the writer while it has room: its own free space,
/// and the share's for an upload.
fn flush_parked(inner: &mut StreamForwarderInner) -> StreamPushResult {
    while inner.direct_writer.as_ref().is_some_and(|writer| writer.room() > 0) {
        let Some(chunk) = inner.unpark_front() else { break };
        let result = write_or_park(inner, chunk);
        if result != StreamPushResult::Ok {
            return result;
        }
    }
    StreamPushResult::Ok
}

/// The cancellation a refused push stands for, or `None` if it was accepted.
fn refusal(result: StreamPushResult) -> Option<Cancellation> {
    match result {
        StreamPushResult::Ok => None,
        StreamPushResult::Full => Some(Cancellation::Overflow),
        StreamPushResult::Closed => Some(Cancellation::Cancelled),
    }
}

fn close_forwarder(fwd: &StreamForwarder, state: &SharedState, stream_id: u32) {
    let mut inner = fwd.borrow_mut();
    inner.closed = true;
    let request = inner.request.take();
    inner.reader.take();
    inner.continuation_context.take();
    // If a direct writer is attached, signal EOF.
    if let Some(writer) = inner.direct_writer.as_ref() {
        writer.close();
    }
    let remove_now = inner.direct_writer.is_some();
    drop(inner);
    if remove_now {
        remove(state, stream_id);
    }
    if let Some(request) = request { request.release(state); }
}

fn error_forwarder(fwd: &StreamForwarder, state: &SharedState, stream_id: u32, msg: &str) {
    // Abort rather than close, so a consumer can tell a producer failure from
    // a clean EOF. `abort` still ends the stream, so the wire path is
    // unchanged: the upstream peer sees a truncated body, since HTTP has no
    // way to retract a response whose head is already on the socket. What it
    // buys is the storage path, where committing the bytes received so far
    // would durably store a prefix of the object as if it were whole.
    //
    // The kernel could additionally emit a response trailer or a TCP RST on
    // the wire path; nothing reads the reason there yet.
    let mut inner = fwd.borrow_mut();
    inner.closed = true;
    inner.error.get_or_insert_with(|| msg.to_string());
    let request = inner.request.take();
    inner.reader.take();
    inner.continuation_context.take();
    if let Some(writer) = inner.direct_writer.as_ref() {
        writer.abort(msg);
    }
    let remove_now = inner.direct_writer.is_some();
    drop(inner);
    if remove_now {
        remove(state, stream_id);
    }
    if let Some(request) = request { request.release(state); }
}

/// End every body this isolate is still forwarding, as a failure carrying
/// `msg`. Called when the isolate is quarantined: no JavaScript runs in it
/// again, so no read loop would ever deliver another chunk or close its body,
/// and a consumer left waiting on one would wait forever. Runs no JavaScript
/// itself; the readers are released, not cancelled.
///
/// A plain body is aborted, so its consumer can tell the cut from a clean end.
/// An RPC stream carries its own end-of-stream protocol, and a client reads a
/// body that stops without its terminal frame as a stream that finished, so
/// it is given the terminal error frame first and then closed - the same end
/// `cancel_reader` gives a cancelled one.
pub(crate) fn fail_all(state: &SharedState, msg: &'static str) {
    let forwarders: Vec<(u32, StreamForwarder)> = state
        .borrow()
        .stream_forwarders
        .iter()
        .map(|(&stream_id, fwd)| (stream_id, fwd.clone()))
        .collect();
    for (stream_id, fwd) in forwarders {
        let (closed, rpc_framing, request_id) = {
            let inner = fwd.borrow();
            let request_id = inner.request.as_ref().map_or(0, |request| request.request_id);
            (inner.closed, inner.rpc_framing, request_id)
        };
        if closed {
            continue;
        }
        if !rpc_framing {
            error_forwarder(&fwd, state, stream_id, msg);
            continue;
        }
        let failure = crate::rpc::dispatch::response::host_error(
            msg,
            crate::rpc::ZsErrorCode::Internal,
        );
        let delivered = match crate::rpc::dispatch::stream::terminal_error(failure, request_id) {
            Ok(frame) => {
                let mut inner = fwd.borrow_mut();
                if let Some(writer) = &inner.direct_writer {
                    matches!(writer.push(frame), StreamPushResult::Ok)
                } else {
                    inner.park_back(frame);
                    true
                }
            }
            Err(_) => false,
        };
        if delivered {
            close_forwarder(&fwd, state, stream_id);
        } else {
            error_forwarder(&fwd, state, stream_id, msg);
        }
    }
}

// ---------------------------------------------------------------------------
// Kernel API — used by `runtime::build_fetch_outcome`
// ---------------------------------------------------------------------------

/// Attach a `direct_writer` to the forwarder. Drains any buffered
/// chunks into the writer; if the forwarder is already closed, calls
/// `writer.close()` so the consumer observes EOF.
///
/// After this call returns, future chunks pushed by the read loop
/// flow direct to the writer's channel.
pub fn attach_writer(state: &SharedState, stream_id: u32, writer: StreamWriter) {
    let Some(fwd) = get(state, stream_id) else {
        // No forwarder — this should not happen if begin_forward returned
        // this stream_id. Close the writer so the consumer terminates
        // cleanly instead of hanging.
        writer.close();
        return;
    };
    let weak_state = Rc::downgrade(state);
    let in_share = fwd.borrow().share.is_some();
    writer.set_consumer_callback(Rc::new(move |event| {
        let Some(state) = weak_state.upgrade() else { return; };
        match event {
            crate::channel::StreamConsumerEvent::Drained => {
                let below_watermark = get(&state, stream_id).is_some_and(|fwd| {
                    fwd.borrow().direct_writer.as_ref()
                        .is_some_and(|writer| writer.buffered_bytes() <= writer.cap() / 4)
                });
                if below_watermark { request_resume(&state, stream_id); }
            }
            crate::channel::StreamConsumerEvent::Closed => request_cancel(&state, stream_id, Cancellation::Cancelled),
        }
        // Bytes left the runtime's upload share: an upload parked for want
        // of share room may now go on.
        if in_share {
            resume_parked_uploads(&state);
        }
    }));
    let (buffered, closed) = {
        let mut inner = fwd.borrow_mut();
        inner.direct_writer = Some(writer);
        let mut taken = std::collections::VecDeque::new();
        while let Some(chunk) = inner.unpark_front() {
            taken.push_back(chunk);
        }
        (taken, inner.closed)
    };
    for chunk in buffered {
        if closed {
            let result = fwd.borrow().direct_writer.as_ref().unwrap().push(chunk);
            if !matches!(result, StreamPushResult::Ok) { break; }
        } else if !push_chunk(&fwd, state, stream_id, chunk) { return; }
    }
    if closed {
        let inner = fwd.borrow();
        let writer = inner.direct_writer.as_ref().unwrap();
        if let Some(error) = &inner.error { writer.abort(error); }
        else { writer.close(); }
        drop(inner);
        remove(state, stream_id);
    } else if !should_pause(&fwd) {
        request_resume(state, stream_id);
    }
}

fn request_cancel(state: &SharedState, stream_id: u32, reason: Cancellation) {
    let Some(fwd) = get(state, stream_id) else { return; };
    {
        let mut inner = fwd.borrow_mut();
        if inner.closed || inner.cancel_requested.is_some() { return; }
        inner.cancel_requested = Some(reason);
        if inner.reader.is_none() && inner.request.is_none() {
            drop(inner);
            close_forwarder(&fwd, state, stream_id);
            return;
        }
    }
    let mut state = state.borrow_mut();
    if !state.forwarder_resumes.contains(&stream_id) {
        state.forwarder_resumes.push_back(stream_id);
    }
    state.notify_pump();
}

fn cancel_reader(
    scope: &mut v8::PinScope,
    state: &SharedState,
    stream_id: u32,
    fwd: &StreamForwarder,
) {
    let (reader, frame, request, reason, rpc_framing, abort_signal, internal) = {
        let mut inner = fwd.borrow_mut();
        (inner.reader.take(), inner.continuation_context.take(), inner.request.take(),
         inner.cancel_requested.take().unwrap_or(Cancellation::Cancelled), inner.rpc_framing,
         inner.abort_signal.take(), inner.internal_reader)
    };
    // Mark closed before abort listeners run. Late reads and reentrant
    // cancellation cannot append chunks or invoke source cleanup again.
    fwd.borrow_mut().closed = true;
    let request_id = request.as_ref().map_or(0, |request| request.request_id);
    if rpc_framing {
        let bytes = crate::rpc::dispatch::stream::terminal_error(reason.response(), request_id);
        let mut inner = fwd.borrow_mut();
        match bytes {
            Ok(bytes) => {
                if let Some(writer) = &inner.direct_writer { let _ = writer.push(bytes); }
                else { inner.buffer.push_back(bytes); }
            }
            Err(message) => {
                if let Some(writer) = &inner.direct_writer { writer.abort(&message); }
            }
        }
    } else if let Some(writer) = &fwd.borrow().direct_writer {
        writer.abort(reason.message());
    }
    let cancel = |scope: &mut v8::PinScope| {
        let reason_value = match aborted_reason(scope, abort_signal.as_ref()) {
            Some(signal_reason) => signal_reason,
            None => reason.exception(scope),
        };
        if let Some(request) = &request {
            request.cancel.cancel();
            v8::tc_scope!(let tc, scope);
            request.signal.abort(tc, reason_value);
        }
        if let Some(reader) = reader {
            cancel_source(scope, reader, reason_value, internal);
        }
    };
    match frame {
        Some(frame) => crate::core::invocation::with_captured_context(scope, &frame, cancel),
        None => cancel(scope),
    }
    close_forwarder(fwd, state, stream_id);
    if let Some(request) = request { request.release(state); }
}

/// End every upload `request_id` started, cancelling each source with
/// `reason`'s exception. An upload belongs to the request that started it:
/// when that request is cancelled or times out, its uploads stop reading
/// their sources and their channels end in failure.
pub(crate) fn cancel_uploads_owned_by(state: &SharedState, request_id: u64, reason: Cancellation) {
    let owned: Vec<u32> = state
        .borrow()
        .stream_forwarders
        .iter()
        .filter(|(_, fwd)| {
            let inner = fwd.borrow();
            !inner.closed && inner.owner_request == Some(request_id)
        })
        .map(|(&stream_id, _)| stream_id)
        .collect();
    for stream_id in owned {
        request_cancel(state, stream_id, reason);
    }
}

/// Re-arm every upload paused with parked bytes. Called when bytes leave the
/// runtime's upload share, so an upload that parked for want of share room
/// is not left waiting on its own, empty, channel.
fn resume_parked_uploads(state: &SharedState) {
    let parked: Vec<u32> = state
        .borrow()
        .stream_forwarders
        .iter()
        .filter(|(_, fwd)| {
            let inner = fwd.borrow();
            inner.share.is_some() && inner.paused && !inner.closed && !inner.buffer.is_empty()
        })
        .map(|(&stream_id, _)| stream_id)
        .collect();
    for stream_id in parked {
        request_resume(state, stream_id);
    }
}

/// The reason of an aborted `fetch` signal: "abort a fetch() call" cancels
/// the request body with the abort's error. `None` if there is no signal or
/// it has not aborted.
fn aborted_reason<'s>(
    scope: &mut v8::PinScope<'s, '_>,
    signal: Option<&v8::Global<v8::Object>>,
) -> Option<v8::Local<'s, v8::Value>> {
    let signal = v8::Local::new(scope, signal?);
    let state = crate::dom::abort_signal::signal_from_obj(scope, signal)?;
    if !state.aborted.get() {
        return None;
    }
    let reason = state.reason.borrow().clone()?;
    Some(v8::Local::new(scope, reason))
}

/// Ask the pump to resume a paused upload forwarder. Called by the consumer of
/// the paired `StreamReader` after it has drained the buffer. Idempotent and
/// cheap: enqueues the `stream_id` (skipping duplicates) and notifies the pump,
/// which calls [`resume_read`] inside its V8 scope. No-op if the forwarder is
/// not paused.
pub fn request_resume(state: &SharedState, stream_id: u32) {
    {
        let Some(fwd) = get(state, stream_id) else { return };
        if !fwd.borrow().paused {
            return;
        }
    }
    let mut s = state.borrow_mut();
    if !s.forwarder_resumes.contains(&stream_id) {
        s.forwarder_resumes.push_back(stream_id);
    }
    s.notify_pump();
}

/// Has the forwarder seen `done: true` from the underlying reader?
/// Used by `classify_stream` to decide whether to treat the response
/// as Complete (sync-completed body) vs Stream (chunks still flowing).
pub fn is_closed(state: &SharedState, stream_id: u32) -> bool {
    get(state, stream_id)
        .map(|fwd| fwd.borrow().closed)
        .unwrap_or(true)
}

/// Drain the current buffered chunks (and remove the forwarder if
/// already closed). Used by `classify_stream` when the body
/// sync-completed during start() — we collect the chunks as the
/// complete-body view and discard the forwarder.
pub fn drain_into_complete(state: &SharedState, stream_id: u32) -> Vec<Vec<u8>> {
    let Some(fwd) = get(state, stream_id) else {
        return Vec::new();
    };
    let chunks: Vec<Vec<u8>> = fwd.borrow_mut().buffer.drain(..).collect();
    // If the forwarder is already closed, no more chunks coming;
    // remove from the registry. Otherwise leave it for build_fetch_outcome
    // to pick up (which will drain any further chunks again before
    // attaching the writer).
    if fwd.borrow().closed {
        remove(state, stream_id);
    }
    chunks
}

// ---------------------------------------------------------------------------
// Upload consumer
// ---------------------------------------------------------------------------

/// Why an upload ended before its source reached end-of-stream.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum UploadError {
    /// The channel refused a chunk: its process-wide byte budget is spent.
    Overflow,
    /// The source failed or was cancelled. The bytes read so far are a
    /// prefix of the body, never the whole of it.
    Failed(String),
}

impl fmt::Display for UploadError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            UploadError::Overflow => f.write_str("upload stream exceeded the buffer backpressure cap"),
            UploadError::Failed(reason) => write!(f, "upload stream failed: {reason}"),
        }
    }
}

/// The consumer side of [`forward_upload`].
///
/// Yields the source's bytes in order, waits (waker-based) while the
/// channel is empty and the source is live, reports a failed source as an
/// error rather than an end, and ends at the source's end-of-stream.
///
/// Draining re-arms the paused producer once the channel is down to a
/// quarter of its cap, so the upload proceeds in bounded-memory waves.
/// Dropping it before the end cancels the source.
///
/// It holds its runtime weakly: an upload stalled inside the HTTP client
/// does not keep an evicted runtime's state alive.
pub struct UploadReader {
    reader: StreamReader,
    state: Weak<RefCell<crate::state::RuntimeState>>,
    stream_id: u32,
}

/// Ends an upload from outside its consumer, for example once the response
/// it belongs to is complete.
#[derive(Clone, Debug)]
pub struct UploadControl {
    state: Weak<RefCell<crate::state::RuntimeState>>,
    stream_id: u32,
}

impl UploadControl {
    /// Stop an upload whose source has not finished: its source is cancelled
    /// and its channel ends in failure. Nothing happens once the source has
    /// ended or the upload is already over.
    pub fn cancel(&self) {
        if let Some(state) = self.state.upgrade() {
            request_cancel(&state, self.stream_id, Cancellation::Cancelled);
        }
    }
}

impl fmt::Debug for UploadReader {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("UploadReader").field("stream_id", &self.stream_id).finish_non_exhaustive()
    }
}

impl UploadReader {
    /// A handle that can end this upload from outside its consumer.
    pub fn control(&self) -> UploadControl {
        UploadControl { state: self.state.clone(), stream_id: self.stream_id }
    }

    /// The next chunk, the failure that ended the upload, or `None` at the
    /// source's end-of-stream.
    pub async fn next_chunk(&mut self) -> Option<Result<Bytes, UploadError>> {
        std::future::poll_fn(|cx| self.poll_next_chunk(cx)).await
    }

    /// Poll form of [`Self::next_chunk`].
    pub fn poll_next_chunk(
        &mut self,
        cx: &mut Context<'_>,
    ) -> Poll<Option<Result<Bytes, UploadError>>> {
        loop {
            if let Some(chunk) = self.reader.pop() {
                // Buffer space was just freed; let the producer refill it.
                self.maybe_resume_producer();
                return Poll::Ready(Some(Ok(Bytes::from(chunk))));
            }
            if self.reader.is_overflow() {
                return Poll::Ready(Some(Err(UploadError::Overflow)));
            }
            // Checked BEFORE `is_done`, which an abort also sets: a source
            // that failed partway must not read as a complete body.
            if let Some(err) = self.reader.error() {
                return Poll::Ready(Some(Err(UploadError::Failed(err))));
            }
            if self.reader.is_done() {
                return Poll::Ready(None);
            }
            // Empty buffer with a live source that may be paused (it pauses
            // at the high-water mark, and a final short chunk can leave it
            // paused with the buffer drained). Nudge a resume before parking
            // so the wait never outlasts data a paused source will not send.
            self.maybe_resume_producer();
            if self.reader.has_data() || self.reader.is_done() {
                continue;
            }
            self.reader.register_waker(cx.waker());
            return Poll::Pending;
        }
    }

    /// Re-arm the paused producer once the channel has drained to a quarter
    /// of its cap (hysteresis against the half-cap pause mark). Idempotent:
    /// `request_resume` does nothing unless the forwarder is paused.
    fn maybe_resume_producer(&self) {
        if self.reader.buffered_bytes() <= self.reader.cap() / 4
            && let Some(state) = self.state.upgrade()
        {
            request_resume(&state, self.stream_id);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::state::RuntimeState;
    use std::collections::HashMap;

    fn test_state() -> SharedState {
        Rc::new(RefCell::new(RuntimeState::new(HashMap::new(), None, None)))
    }

    #[test]
    fn closed_forwarder_with_attached_writer_is_removed_from_registry() {
        let state = test_state();
        let stream_id = 41;
        let fwd: StreamForwarder = Rc::new(RefCell::new(StreamForwarderInner::default()));
        register(&state, stream_id, fwd.clone());

        let (writer, _reader) = crate::channel::stream_buffer();
        attach_writer(&state, stream_id, writer);
        close_forwarder(&fwd, &state, stream_id);

        assert!(
            get(&state, stream_id).is_none(),
            "closed forwarder should not remain registered after EOF"
        );
    }

    #[test]
    fn writer_overflow_closes_forwarder_and_unregisters_it() {
        let state = test_state();
        let stream_id = 42;
        let fwd: StreamForwarder = Rc::new(RefCell::new(StreamForwarderInner::default()));
        register(&state, stream_id, fwd.clone());

        let (writer, reader) = crate::channel::stream_buffer_with_cap(4);
        attach_writer(&state, stream_id, writer);

        let accepted = push_chunk(&fwd, &state, stream_id, vec![0u8; 8]);
        assert!(!accepted, "overflow should stop the forwarder immediately");
        assert!(reader.is_overflow(), "downstream reader should observe overflow");
        assert!(fwd.borrow().closed, "forwarder should be marked closed");
        assert!(
            get(&state, stream_id).is_none(),
            "overflowed forwarder should be removed from the registry"
        );
    }

    #[test]
    fn failure_before_writer_attachment_remains_a_transport_error() {
        let state = test_state();
        let stream_id = 43;
        let fwd = Rc::new(RefCell::new(StreamForwarderInner::default()));
        register(&state, stream_id, fwd.clone());
        error_forwarder(&fwd, &state, stream_id, "reader failed");
        let (writer, reader) = crate::channel::stream_buffer();
        attach_writer(&state, stream_id, writer);
        assert!(reader.is_done());
        assert_eq!(reader.error().as_deref(), Some("reader failed"));
        assert!(get(&state, stream_id).is_none());
    }
}
