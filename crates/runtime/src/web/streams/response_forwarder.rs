//! Rust-side response-body forwarder.
//!
//! Replaces the JS pump in `embed/fetch.js`'s `__zsBeginStreamForward`.
//! When the kernel decides a Response with a ReadableStream body should
//! ship to the wire, it calls `begin_forward(scope, response_obj)`. We:
//!
//!   1. Allocate a stream_id (used by the kernel to match this forward
//!      to the eventual `direct_writer` attachment).
//!   2. Lock the body via `getReader()` (the public spec API, so any
//!      class that implements the WHATWG Streams surface — native or
//!      user-defined — works).
//!   3. Drive `reader.read()` in a Rust-side promise-reaction loop:
//!      each `read()` returns a Promise; we attach `.then(on_chunk,
//!      on_error)` callbacks. on_chunk normalises the chunk to bytes
//!      and pushes through the forwarder; on_error errors it; the
//!      `done` flag closes it.
//!   4. The forwarder either buffers chunks (until the kernel attaches
//!      a `direct_writer`) or pushes them straight to that writer's
//!      channel.
//!
//! No JS-visible namespace, no `__streams.{create,enqueue,close,error}`,
//! no `OpResult::StreamChunk` round-trip. Everything stays in V8 memory
//! + a Rust-owned channel.
//!
//! ## Why getReader (not native-internal API)
//!
//! `crate::streams::readable_default_reader::acquire_readable_stream_default_reader`
//! is a faster path that bypasses the JS-visible reader class — but it
//! only works on NATIVE ReadableStreams. Custom stream classes that
//! satisfy the spec surface but aren't backed by `RSState` would
//! fail the brand check. The kernel must accept any spec-compliant
//! Response body, so we go through the public surface.
//!
//! The cost is one extra V8 frame per chunk: getReader + each read()
//! is a method call. For a 1MB streamed response in 64KB chunks that's
//! 16 extra V8 frames; negligible compared to the actual pump work.

use std::cell::RefCell;
use std::collections::VecDeque;
use std::rc::Rc;

use crate::channel::{StreamPushResult, StreamWriter};
use crate::state::SharedState;

// ---------------------------------------------------------------------------
// Forwarder state
// ---------------------------------------------------------------------------

/// Shared state between the Rust promise-reaction callbacks and the
/// kernel's `attach_writer` / `is_closed` queries. Lives in a
/// `Rc<RefCell<>>` so the JS callbacks (which V8 keeps alive via the
/// pending `read()` Promise's reaction list) can mutate it without
/// crossing thread boundaries.
pub struct ResponseForwarderInner {
    /// Buffered chunks not yet drained into a `direct_writer`. Empty
    /// once a writer is attached and chunks flow direct.
    pub buffer: VecDeque<Vec<u8>>,
    /// True once `read()` returned `{done: true}` OR a read rejected.
    pub closed: bool,
    /// Kernel-attached writer. When `None`, chunks queue in `buffer`.
    /// When `Some`, `buffer` MUST be empty (the kernel drains it on
    /// attach) and incoming chunks push straight to the writer's
    /// channel.
    pub direct_writer: Option<StreamWriter>,
}

impl Default for ResponseForwarderInner {
    fn default() -> Self {
        Self {
            buffer: VecDeque::new(),
            closed: false,
            direct_writer: None,
        }
    }
}

pub type ResponseForwarder = Rc<RefCell<ResponseForwarderInner>>;

// ---------------------------------------------------------------------------
// Registry — stream_id → ResponseForwarder
// ---------------------------------------------------------------------------

/// Insert a forwarder into the per-isolate map. Caller must hold the
/// SharedState mutex (we do that internally).
fn register(state: &SharedState, stream_id: u32, fwd: ResponseForwarder) {
    state.borrow_mut().response_forwarders.insert(stream_id, fwd);
}

/// Look up a forwarder by stream_id.
pub fn get(state: &SharedState, stream_id: u32) -> Option<ResponseForwarder> {
    state.borrow().response_forwarders.get(&stream_id).cloned()
}

/// Remove a forwarder from the registry. Called by the kernel after
/// the writer has been attached + the buffered chunks drained, or
/// after the stream errors.
pub fn remove(state: &SharedState, stream_id: u32) {
    state.borrow_mut().response_forwarders.remove(&stream_id);
}

// ---------------------------------------------------------------------------
// begin_forward — entry from `http::inspect_response`
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
    if let Some(existing) = response_obj.get(scope, id_key.into()) {
        if !existing.is_undefined() {
            if let Some(id) = existing.uint32_value(scope) {
                if id != u32::MAX {
                    return Ok(id);
                }
            }
        }
    }

    // Read response.body.
    let body_key = v8::String::new(scope, "body").unwrap();
    let body_v = response_obj
        .get(scope, body_key.into())
        .ok_or_else(|| "begin_forward: response.body access threw".to_string())?;
    let body_obj = v8::Local::<v8::Object>::try_from(body_v)
        .map_err(|_| "begin_forward: response.body is not an object".to_string())?;

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

    // Allocate stream_id.
    let state: SharedState = scope
        .get_slot::<SharedState>()
        .expect("RuntimeState not in isolate slot")
        .clone();
    let stream_id = state.borrow_mut().alloc_stream_id();

    // Stamp the id on the response so the kernel can read it later
    // for idempotent re-inspect — and so build_fetch_outcome can match
    // this forwarder to the writer attach.
    let id_v = v8::Integer::new_from_unsigned(scope, stream_id);
    response_obj.set(scope, id_key.into(), id_v.into());

    // Allocate forwarder and register.
    let fwd: ResponseForwarder = Rc::new(RefCell::new(ResponseForwarderInner::default()));
    register(&state, stream_id, fwd.clone());

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
    fwd: ResponseForwarder,
    stream_id: u32,
    state: SharedState,
) {
    let reader = v8::Local::new(scope, &reader_global);
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

    // Build on_chunk + on_error closures. We stash all the captures
    // we'll need in External-backed v8::Functions so the promise's
    // reaction list keeps them alive without us holding Rc<>s on the
    // Rust side.
    let on_chunk = make_on_chunk_callback(scope, reader_global.clone(), fwd.clone(), stream_id, state.clone());
    let on_error = make_on_error_callback(scope, fwd, stream_id, state);

    let promise = v8::Local::new(scope, &promise_global);
    promise.then2(scope, on_chunk, on_error);
}

// ---------------------------------------------------------------------------
// on_chunk / on_error — promise-reaction callbacks
// ---------------------------------------------------------------------------

/// Captures for the on_chunk callback. Stored in an External tied to
/// the v8::Function's data slot.
struct OnChunkCaptures {
    reader: v8::Global<v8::Object>,
    fwd: ResponseForwarder,
    stream_id: u32,
    state: SharedState,
}

/// Captures for the on_error callback.
struct OnErrorCaptures {
    fwd: ResponseForwarder,
    stream_id: u32,
    state: SharedState,
}

fn make_on_chunk_callback<'s>(
    scope: &mut v8::PinScope<'s, '_>,
    reader: v8::Global<v8::Object>,
    fwd: ResponseForwarder,
    stream_id: u32,
    state: SharedState,
) -> v8::Local<'s, v8::Function> {
    let captures = Box::new(OnChunkCaptures { reader, fwd, stream_id, state });
    let raw = Box::into_raw(captures);
    let raw_addr = raw as usize;
    let ext = v8::External::new(scope, raw as *mut std::ffi::c_void);

    let tmpl = v8::FunctionTemplate::builder(on_chunk_callback)
        .data(ext.into())
        .build(scope);
    let f = tmpl.get_function(scope).unwrap();

    // Tie cleanup to the function's lifetime: when V8 GCs the wrapper
    // (which it will once the promise's reaction is consumed), we
    // drop the Box.
    let weak = v8::Weak::with_guaranteed_finalizer(
        scope,
        f,
        Box::new(move || unsafe {
            drop(Box::from_raw(raw_addr as *mut OnChunkCaptures));
        }),
    );
    std::mem::forget(weak);
    f
}

fn make_on_error_callback<'s>(
    scope: &mut v8::PinScope<'s, '_>,
    fwd: ResponseForwarder,
    stream_id: u32,
    state: SharedState,
) -> v8::Local<'s, v8::Function> {
    let captures = Box::new(OnErrorCaptures { fwd, stream_id, state });
    let raw = Box::into_raw(captures);
    let raw_addr = raw as usize;
    let ext = v8::External::new(scope, raw as *mut std::ffi::c_void);

    let tmpl = v8::FunctionTemplate::builder(on_error_callback)
        .data(ext.into())
        .build(scope);
    let f = tmpl.get_function(scope).unwrap();
    let weak = v8::Weak::with_guaranteed_finalizer(
        scope,
        f,
        Box::new(move || unsafe {
            drop(Box::from_raw(raw_addr as *mut OnErrorCaptures));
        }),
    );
    std::mem::forget(weak);
    f
}

fn on_chunk_callback(
    scope: &mut v8::PinScope,
    args: v8::FunctionCallbackArguments,
    _rv: v8::ReturnValue,
) {
    // Recover captures from the data slot.
    let data = args.data();
    let ext = match v8::Local::<v8::External>::try_from(data) {
        Ok(e) => e,
        Err(_) => return,
    };
    let captures: &OnChunkCaptures = unsafe { &*(ext.value() as *const OnChunkCaptures) };

    // The argument is `{ value, done }` (the Promise resolution of
    // `reader.read()`).
    let result = args.get(0);
    let result_obj = match v8::Local::<v8::Object>::try_from(result) {
        Ok(o) => o,
        Err(_) => {
            error_forwarder(
                &captures.fwd,
                &captures.state,
                captures.stream_id,
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
        close_forwarder(&captures.fwd, &captures.state, captures.stream_id);
        return;
    }

    let value_key = v8::String::new(scope, "value").unwrap();
    let value = match result_obj.get(scope, value_key.into()) {
        Some(v) => v,
        None => {
            error_forwarder(
                &captures.fwd,
                &captures.state,
                captures.stream_id,
                "read() result has no `value`",
            );
            return;
        }
    };

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

    if !push_chunk(&captures.fwd, &captures.state, captures.stream_id, bytes) {
        return;
    }

    // Re-arm: schedule the next read. We clone the captures' fields
    // because schedule_next_read consumes them; the captures struct
    // itself stays alive for as long as the original Function does.
    schedule_next_read(
        scope,
        captures.reader.clone(),
        captures.fwd.clone(),
        captures.stream_id,
        captures.state.clone(),
    );
}

fn on_error_callback(
    scope: &mut v8::PinScope,
    args: v8::FunctionCallbackArguments,
    _rv: v8::ReturnValue,
) {
    let data = args.data();
    let ext = match v8::Local::<v8::External>::try_from(data) {
        Ok(e) => e,
        Err(_) => return,
    };
    let captures: &OnErrorCaptures = unsafe { &*(ext.value() as *const OnErrorCaptures) };

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

    error_forwarder(&captures.fwd, &captures.state, captures.stream_id, &msg);
}

// ---------------------------------------------------------------------------
// Forwarder I/O — used by the callbacks above
// ---------------------------------------------------------------------------

fn push_chunk(
    fwd: &ResponseForwarder,
    state: &SharedState,
    stream_id: u32,
    data: Vec<u8>,
) -> bool {
    let mut inner = fwd.borrow_mut();
    if inner.closed {
        return false;
    }
    if let Some(writer) = inner.direct_writer.as_ref() {
        match writer.push(data) {
            StreamPushResult::Ok => true,
            StreamPushResult::Closed | StreamPushResult::Full => {
                drop(inner);
                close_forwarder(fwd, state, stream_id);
                false
            }
        }
    } else {
        inner.buffer.push_back(data);
        true
    }
}

fn close_forwarder(fwd: &ResponseForwarder, state: &SharedState, stream_id: u32) {
    let mut inner = fwd.borrow_mut();
    inner.closed = true;
    // If a direct writer is attached, signal EOF.
    if let Some(writer) = inner.direct_writer.as_ref() {
        writer.close();
    }
    let remove_now = inner.direct_writer.is_some();
    drop(inner);
    if remove_now {
        remove(state, stream_id);
    }
}

fn error_forwarder(
    fwd: &ResponseForwarder,
    state: &SharedState,
    stream_id: u32,
    _msg: &str,
) {
    // For the wire path an error is functionally equivalent to a
    // close (the TCP layer just sees EOF — the upstream peer won't
    // receive a structured error, only a truncated body). Future work:
    // surface error info via the StreamWriter so the kernel can emit
    // a response trailer or a tcp RST.
    close_forwarder(fwd, state, stream_id);
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
    let drained_or_closed = {
        let mut inner = fwd.borrow_mut();
        for chunk in inner.buffer.drain(..) {
            let _ = writer.push(chunk);
        }
        if inner.closed {
            writer.close();
            true
        } else {
            inner.direct_writer = Some(writer);
            false
        }
    };
    if drained_or_closed {
        // Forwarder is done — no more chunks coming. Drop from registry.
        remove(state, stream_id);
    }
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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::state::RuntimeState;
    use std::collections::HashMap;

    fn test_state() -> SharedState {
        Rc::new(RefCell::new(RuntimeState::new(HashMap::new(), None)))
    }

    #[test]
    fn closed_forwarder_with_attached_writer_is_removed_from_registry() {
        let state = test_state();
        let stream_id = 41;
        let fwd: ResponseForwarder = Rc::new(RefCell::new(ResponseForwarderInner::default()));
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
        let fwd: ResponseForwarder = Rc::new(RefCell::new(ResponseForwarderInner::default()));
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
}
