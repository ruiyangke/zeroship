//! ReadableStream native backing — V8 callbacks for stream lifecycle.
//!
//! Provides five native callbacks registered on `globalThis.__streams`:
//! - `create()` — allocate a stream_id, return u32
//! - `read(stream_id)` — create PromiseResolver, return Promise
//! - `enqueue(stream_id, Uint8Array)` — resolve pending read or buffer chunk
//! - `close(stream_id)` — mark closed, resolve pending read with {done: true}
//! - `error(stream_id, msg)` — mark closed, reject pending read
//!
//! The synchronous fast-path (enqueue while read is pending) resolves the
//! promise immediately without going through the event channel. This handles
//! the common SSE pattern where `controller.enqueue()` is called from a
//! setTimeout callback while `reader.read()` is awaited.

use crate::state::{SharedState, StreamState};

/// Resolve a PromiseResolver with `{value: Uint8Array(data), done: false}`.
fn resolve_with_chunk(
    scope: &mut v8::PinScope,
    resolver: v8::Local<v8::PromiseResolver>,
    data: &[u8],
) {
    let result = v8::Object::new(scope);
    let done_key = v8::String::new(scope, "done").unwrap();
    result.set(scope, done_key.into(), v8::Boolean::new(scope, false).into());

    let value_key = v8::String::new(scope, "value").unwrap();
    let ab = v8::ArrayBuffer::new(scope, data.len());
    let store = ab.get_backing_store();
    for (i, &b) in data.iter().enumerate() {
        store[i].set(b);
    }
    let uint8 = v8::Uint8Array::new(scope, ab, 0, data.len()).unwrap();
    result.set(scope, value_key.into(), uint8.into());

    resolver.resolve(scope, result.into());
}

/// Resolve a PromiseResolver with `{value: undefined, done: true}`.
fn resolve_with_done(
    scope: &mut v8::PinScope,
    resolver: v8::Local<v8::PromiseResolver>,
) {
    let result = v8::Object::new(scope);
    let done_key = v8::String::new(scope, "done").unwrap();
    result.set(scope, done_key.into(), v8::Boolean::new(scope, true).into());
    let value_key = v8::String::new(scope, "value").unwrap();
    result.set(scope, value_key.into(), v8::undefined(scope).into());
    resolver.resolve(scope, result.into());
}

// ---------------------------------------------------------------------------
// stream_create — allocate a new stream_id
// ---------------------------------------------------------------------------

pub(crate) fn stream_create_callback(
    scope: &mut v8::PinScope,
    _args: v8::FunctionCallbackArguments,
    mut rv: v8::ReturnValue,
) {
    let state: SharedState = scope
        .get_slot::<SharedState>()
        .expect("RuntimeState not in isolate slot")
        .clone();
    let mut s = state.borrow_mut();
    let id = s.next_stream_id;
    s.next_stream_id += 1;
    s.streams.insert(id, StreamState {
        pending_read: None,
        buffer: Vec::new(),
        closed: false,
    });
    rv.set(v8::Integer::new_from_unsigned(scope, id).into());
}

// ---------------------------------------------------------------------------
// stream_read — returns Promise<{value, done}>
// ---------------------------------------------------------------------------

pub(crate) fn stream_read_callback(
    scope: &mut v8::PinScope,
    args: v8::FunctionCallbackArguments,
    mut rv: v8::ReturnValue,
) {
    let stream_id = args.get(0).uint32_value(scope).unwrap_or(0);

    let resolver = v8::PromiseResolver::new(scope).unwrap();
    let promise = resolver.get_promise(scope);

    let state: SharedState = scope
        .get_slot::<SharedState>()
        .expect("RuntimeState not in isolate slot")
        .clone();
    let mut s = state.borrow_mut();

    // Lazy-create StreamState if it doesn't exist yet (streaming fetch path:
    // stream_id is allocated in the fetch callback but StreamState is deferred).
    let stream = s.streams.entry(stream_id).or_insert_with(|| {
        crate::state::StreamState {
            pending_read: None,
            buffer: Vec::new(),
            closed: false,
        }
    });

    if !stream.buffer.is_empty() {
        // Buffered chunk available — resolve immediately.
        let data = stream.buffer.remove(0);
        drop(s);
        resolve_with_chunk(scope, resolver, &data);
    } else if stream.closed {
        // Stream already closed — resolve with {done: true}.
        drop(s);
        resolve_with_done(scope, resolver);
    } else {
        // No data available — store resolver for later.
        stream.pending_read = Some(v8::Global::new(scope, resolver));
    }

    rv.set(promise.into());
}

// ---------------------------------------------------------------------------
// stream_enqueue — push a chunk to the stream
// ---------------------------------------------------------------------------

pub(crate) fn stream_enqueue_callback(
    scope: &mut v8::PinScope,
    args: v8::FunctionCallbackArguments,
    _rv: v8::ReturnValue,
) {
    let stream_id = args.get(0).uint32_value(scope).unwrap_or(0);

    // Extract bytes from the Uint8Array argument.
    let data = if let Ok(view) = v8::Local::<v8::ArrayBufferView>::try_from(args.get(1)) {
        let mut buf = vec![0u8; view.byte_length()];
        view.copy_contents(&mut buf);
        buf
    } else {
        // Fallback: convert to string and encode as UTF-8.
        let s = args.get(1).to_rust_string_lossy(scope);
        s.into_bytes()
    };

    let state: SharedState = scope
        .get_slot::<SharedState>()
        .expect("RuntimeState not in isolate slot")
        .clone();

    // Synchronous fast-path: if there's a pending read, resolve it immediately.
    let pending = {
        let mut s = state.borrow_mut();
        s.streams.get_mut(&stream_id).and_then(|stream| stream.pending_read.take())
    };

    if let Some(resolver_global) = pending {
        let resolver = v8::Local::new(scope, &resolver_global);
        resolve_with_chunk(scope, resolver, &data);
    } else {
        // No pending read — buffer the chunk for later.
        let mut s = state.borrow_mut();
        if let Some(stream) = s.streams.get_mut(&stream_id) {
            stream.buffer.push(data);
        }
    }
}

// ---------------------------------------------------------------------------
// stream_close — mark stream as closed
// ---------------------------------------------------------------------------

pub(crate) fn stream_close_callback(
    scope: &mut v8::PinScope,
    args: v8::FunctionCallbackArguments,
    _rv: v8::ReturnValue,
) {
    let stream_id = args.get(0).uint32_value(scope).unwrap_or(0);

    let state: SharedState = scope
        .get_slot::<SharedState>()
        .expect("RuntimeState not in isolate slot")
        .clone();

    let pending = {
        let mut s = state.borrow_mut();
        if let Some(stream) = s.streams.get_mut(&stream_id) {
            stream.closed = true;
            stream.pending_read.take()
        } else {
            None
        }
    };

    // If there's a pending read, resolve it with {done: true}.
    if let Some(resolver_global) = pending {
        let resolver = v8::Local::new(scope, &resolver_global);
        resolve_with_done(scope, resolver);
    }
}

// ---------------------------------------------------------------------------
// stream_error — mark stream as closed with an error
// ---------------------------------------------------------------------------

pub(crate) fn stream_error_callback(
    scope: &mut v8::PinScope,
    args: v8::FunctionCallbackArguments,
    _rv: v8::ReturnValue,
) {
    let stream_id = args.get(0).uint32_value(scope).unwrap_or(0);
    let err_msg = args.get(1).to_rust_string_lossy(scope);

    let state: SharedState = scope
        .get_slot::<SharedState>()
        .expect("RuntimeState not in isolate slot")
        .clone();

    let pending = {
        let mut s = state.borrow_mut();
        if let Some(stream) = s.streams.get_mut(&stream_id) {
            stream.closed = true;
            stream.pending_read.take()
        } else {
            None
        }
    };

    // If there's a pending read, reject it with the error.
    if let Some(resolver_global) = pending {
        let resolver = v8::Local::new(scope, &resolver_global);
        let msg = v8::String::new(scope, &err_msg).unwrap();
        let err = v8::Exception::error(scope, msg);
        resolver.reject(scope, err);
    }
}

// ---------------------------------------------------------------------------
// push_stream_chunk — called from event loop to deliver background I/O chunks
// ---------------------------------------------------------------------------

/// Push a chunk from the event loop channel into a stream's buffer or pending reader.
///
/// When `done` is true, the stream is closed. If `data` is non-empty AND `done`
/// is true, the data is delivered first, then the stream is closed.
///
/// This is the bridge between background tokio tasks (streaming fetch) and the
/// V8 ReadableStream infrastructure.
pub(crate) fn push_stream_chunk(
    scope: &mut v8::PinScope,
    state: &SharedState,
    stream_id: u32,
    data: &[u8],
    done: bool,
) {
    // Deliver data chunk (if non-empty)
    if !data.is_empty() {
        let pending = {
            let mut s = state.borrow_mut();
            // Lazy-create StreamState if it doesn't exist yet
            let stream = s.streams.entry(stream_id).or_insert_with(|| {
                crate::state::StreamState {
                    pending_read: None,
                    buffer: Vec::new(),
                    closed: false,
                }
            });
            stream.pending_read.take()
        };

        if let Some(resolver_global) = pending {
            let resolver = v8::Local::new(scope, &resolver_global);
            resolve_with_chunk(scope, resolver, data);
        } else {
            let mut s = state.borrow_mut();
            if let Some(stream) = s.streams.get_mut(&stream_id) {
                stream.buffer.push(data.to_vec());
            }
        }
    }

    // Close stream if done
    if done {
        let pending = {
            let mut s = state.borrow_mut();
            if let Some(stream) = s.streams.get_mut(&stream_id) {
                stream.closed = true;
                stream.pending_read.take()
            } else {
                None
            }
        };

        if let Some(resolver_global) = pending {
            let resolver = v8::Local::new(scope, &resolver_global);
            resolve_with_done(scope, resolver);
        }
    }
}
