//! V8 callbacks for `zeroship.storage.*` methods.
//!
//! Each callback mirrors the plugin-db pattern: parse args, allocate a
//! promise, push an async op into the runtime pump's spawned-ops queue,
//! return the promise. The pump resolves/rejects via OpResult.

use std::cell::RefCell;
use std::collections::HashMap;
use std::rc::Rc;
use std::sync::Arc;

use base64::Engine;
use serde_json::json;
use zeroship_runtime::channel::{stream_buffer, StreamReader};
use zeroship_runtime::state::{OpError, OpResult, ResolveValue, SharedState};
use zeroship_runtime::streams::response_forwarder;

use crate::backend::{BoxByteStream, ChunkResult, ChunkSource, ObjectMeta};
use crate::{Backend, STORAGE_BACKEND};

// ---------------------------------------------------------------------------
// Shared helpers
// ---------------------------------------------------------------------------

fn require_string_arg(
    scope: &mut v8::PinScope<'_, '_>,
    args: &v8::FunctionCallbackArguments,
    index: i32,
    name: &str,
) -> Option<String> {
    let val = if args.length() > index { args.get(index) } else { return throw_type(scope, name); };
    if val.is_null_or_undefined() { return throw_type(scope, name); }
    let s = val.to_rust_string_lossy(scope);
    if s.is_empty() { return throw_type(scope, name); }
    Some(s)
}

fn optional_string_arg(
    scope: &mut v8::PinScope<'_, '_>,
    args: &v8::FunctionCallbackArguments,
    index: i32,
) -> Option<String> {
    if args.length() <= index { return None; }
    let val = args.get(index);
    if val.is_null_or_undefined() { return None; }
    let s = val.to_rust_string_lossy(scope);
    if s.is_empty() { None } else { Some(s) }
}

fn throw_type(scope: &mut v8::PinScope, arg_name: &str) -> Option<String> {
    let msg = v8::String::new(scope, &format!("storage: missing required argument '{arg_name}'")).unwrap();
    let exc = v8::Exception::type_error(scope, msg);
    scope.throw_exception(exc);
    None
}

fn get_app_id(state: &SharedState) -> String {
    state
        .borrow()
        .env_vars
        .get("APP_ID")
        .cloned()
        .unwrap_or_else(|| "default".to_string())
}

fn setup_promise<'s>(
    scope: &mut v8::PinScope<'s, '_>,
    state: &SharedState,
) -> (u32, Option<u64>, v8::Local<'s, v8::Promise>) {
    let resolver = v8::PromiseResolver::new(scope).unwrap();
    let promise = resolver.get_promise(scope);
    let global_resolver = v8::Global::new(scope, resolver);

    let mut s = state.borrow_mut();
    let op_id = s.next_op_id;
    s.next_op_id += 1;
    s.pending_resolvers.insert(op_id, global_resolver);
    let request_id = s.executing_request_id;

    (op_id, request_id, promise)
}

fn current_backend() -> Result<Arc<dyn Backend>, String> {
    STORAGE_BACKEND.with(|c| c.borrow().as_ref().map(Arc::clone))
        .ok_or_else(|| "storage: not configured — StoragePlugin not registered".to_string())
}

// ---------------------------------------------------------------------------
// put(bucket, key, bytesBase64, contentType?)
// ---------------------------------------------------------------------------

pub fn put(
    scope: &mut v8::PinScope,
    args: v8::FunctionCallbackArguments,
    mut rv: v8::ReturnValue,
) {
    let state: SharedState = scope.get_slot::<SharedState>().expect("state").clone();

    let Some(bucket) = require_string_arg(scope, &args, 0, "bucket") else { return };
    let Some(key) = require_string_arg(scope, &args, 1, "key") else { return };
    let Some(b64) = require_string_arg(scope, &args, 2, "bytesBase64") else { return };
    let content_type = optional_string_arg(scope, &args, 3);

    let app_id = get_app_id(&state);
    let (op_id, request_id, promise) = setup_promise(scope, &state);

    let bytes = match base64::engine::general_purpose::STANDARD.decode(b64.as_bytes()) {
        Ok(b) => b,
        Err(e) => {
            state.borrow_mut().spawned_ops.push(Box::pin(async move {
                OpResult::Failed {
                    op_id,
                    error: format!("storage: invalid base64: {e}"),
                    request_id,
                }
            }));
            rv.set(promise.into());
            return;
        }
    };

    let backend = match current_backend() {
        Ok(r) => r,
        Err(e) => {
            state.borrow_mut().spawned_ops.push(Box::pin(async move {
                OpResult::Failed { op_id, error: e, request_id }
            }));
            rv.set(promise.into());
            return;
        }
    };

    state.borrow_mut().spawned_ops.push(Box::pin(async move {
        match backend.put(&app_id, &bucket, &key, &bytes, content_type.as_deref()).await {
            Ok(size) => OpResult::Completed {
                op_id,
                value: json!({ "bucket": bucket, "key": key, "size": size }).to_string(),
                request_id,
            },
            Err(e) => OpResult::Failed { op_id, error: e, request_id },
        }
    }));

    rv.set(promise.into());
}

// ---------------------------------------------------------------------------
// get(bucket, key) → { bytesBase64, contentType, size } | null
// ---------------------------------------------------------------------------

pub fn get(
    scope: &mut v8::PinScope,
    args: v8::FunctionCallbackArguments,
    mut rv: v8::ReturnValue,
) {
    let state: SharedState = scope.get_slot::<SharedState>().expect("state").clone();

    let Some(bucket) = require_string_arg(scope, &args, 0, "bucket") else { return };
    let Some(key) = require_string_arg(scope, &args, 1, "key") else { return };

    let app_id = get_app_id(&state);
    let (op_id, request_id, promise) = setup_promise(scope, &state);

    let backend = match current_backend() {
        Ok(r) => r,
        Err(e) => {
            state.borrow_mut().spawned_ops.push(Box::pin(async move {
                OpResult::Failed { op_id, error: e, request_id }
            }));
            rv.set(promise.into());
            return;
        }
    };

    let max_bytes = crate::limits::max_object_bytes();
    state.borrow_mut().spawned_ops.push(Box::pin(async move {
        match backend.get(&app_id, &bucket, &key, max_bytes).await {
            Ok(None) => OpResult::Completed {
                op_id,
                value: "null".into(),
                request_id,
            },
            Ok(Some((bytes, meta))) => {
                let b64 = base64::engine::general_purpose::STANDARD.encode(&bytes);
                let out = json!({
                    "bytesBase64": b64,
                    "contentType": meta.content_type,
                    "size": meta.size,
                });
                OpResult::Completed { op_id, value: out.to_string(), request_id }
            }
            Err(e) => OpResult::Failed { op_id, error: e, request_id },
        }
    }));

    rv.set(promise.into());
}

// ---------------------------------------------------------------------------
// delete(bucket, key) → { deleted: bool }
// ---------------------------------------------------------------------------

pub fn delete(
    scope: &mut v8::PinScope,
    args: v8::FunctionCallbackArguments,
    mut rv: v8::ReturnValue,
) {
    let state: SharedState = scope.get_slot::<SharedState>().expect("state").clone();

    let Some(bucket) = require_string_arg(scope, &args, 0, "bucket") else { return };
    let Some(key) = require_string_arg(scope, &args, 1, "key") else { return };

    let app_id = get_app_id(&state);
    let (op_id, request_id, promise) = setup_promise(scope, &state);

    let backend = match current_backend() {
        Ok(r) => r,
        Err(e) => {
            state.borrow_mut().spawned_ops.push(Box::pin(async move {
                OpResult::Failed { op_id, error: e, request_id }
            }));
            rv.set(promise.into());
            return;
        }
    };

    state.borrow_mut().spawned_ops.push(Box::pin(async move {
        match backend.delete(&app_id, &bucket, &key).await {
            Ok(deleted) => OpResult::Completed {
                op_id,
                value: json!({ "deleted": deleted }).to_string(),
                request_id,
            },
            Err(e) => OpResult::Failed { op_id, error: e, request_id },
        }
    }));

    rv.set(promise.into());
}

// ---------------------------------------------------------------------------
// list(bucket, prefix?) → [{ key, size, modifiedAt }]
// ---------------------------------------------------------------------------

pub fn list(
    scope: &mut v8::PinScope,
    args: v8::FunctionCallbackArguments,
    mut rv: v8::ReturnValue,
) {
    let state: SharedState = scope.get_slot::<SharedState>().expect("state").clone();

    let Some(bucket) = require_string_arg(scope, &args, 0, "bucket") else { return };
    let prefix = optional_string_arg(scope, &args, 1).unwrap_or_default();

    let app_id = get_app_id(&state);
    let (op_id, request_id, promise) = setup_promise(scope, &state);

    let backend = match current_backend() {
        Ok(r) => r,
        Err(e) => {
            state.borrow_mut().spawned_ops.push(Box::pin(async move {
                OpResult::Failed { op_id, error: e, request_id }
            }));
            rv.set(promise.into());
            return;
        }
    };

    state.borrow_mut().spawned_ops.push(Box::pin(async move {
        match backend.list(&app_id, &bucket, &prefix).await {
            Ok(entries) => {
                let arr: Vec<serde_json::Value> = entries.into_iter().map(|e| {
                    let modified = e.modified_at
                        .duration_since(std::time::UNIX_EPOCH)
                        .map(|d| d.as_millis() as u64)
                        .unwrap_or(0);
                    json!({ "key": e.key, "size": e.size, "modifiedAt": modified })
                }).collect();
                OpResult::Completed {
                    op_id,
                    value: serde_json::Value::Array(arr).to_string(),
                    request_id,
                }
            }
            Err(e) => OpResult::Failed { op_id, error: e, request_id },
        }
    }));

    rv.set(promise.into());
}

// ===========================================================================
// Streaming through V8 — see the proposal's
// "env.storage streaming through V8" section.
//
// Upload  (`putStream`): consume an app-supplied V8 ReadableStream via the
//   runtime's `response_forwarder` pump (getReader + promise-reaction read
//   loop into a Rust `StreamWriter`). A spawned op drains the paired
//   `StreamReader` and feeds chunks to `Backend::put_stream` → S3 multipart
//   (or LocalFs temp-file + rename). Memory is bounded by the part size on
//   upload and the StreamWriter backpressure cap, not the object size.
//
// Download (`getStream` + `readChunk` + `cancelStream`): `getStream` opens a
//   `Backend::get_stream` and parks the `(meta, source)` in a per-isolate
//   registry under a fresh id, resolving `{ streamId, contentType, size }`
//   (or `null`). The `@zeroship/storage` SDK builds a `new ReadableStream`
//   whose `pull` calls `readChunk(streamId)` — each call pulls the next
//   `Backend::get_stream` chunk and resolves a `Uint8Array` (or `undefined`
//   at EOF). `cancelStream` drops a half-read source.
// ===========================================================================

thread_local! {
    /// Per-isolate registry of in-flight download streams, keyed by id.
    /// `Rc<RefCell<Option<…>>>` so `readChunk` can take the source out for
    /// the duration of an async pull and put it back, without holding a
    /// `RefCell` borrow across the await.
    static GET_STREAMS: RefCell<HashMap<u32, Rc<RefCell<Option<BoxByteStream>>>>> =
        RefCell::new(HashMap::new());
    /// Monotonic id source for download streams (per isolate).
    static NEXT_GET_STREAM_ID: RefCell<u32> = const { RefCell::new(1) };
}

fn alloc_get_stream_id() -> u32 {
    NEXT_GET_STREAM_ID.with(|c| {
        let mut n = c.borrow_mut();
        let id = *n;
        *n = n.wrapping_add(1).max(1);
        id
    })
}

fn drop_get_stream(stream_id: u32) {
    GET_STREAMS.with(|m| {
        m.borrow_mut().remove(&stream_id);
    });
}

/// A [`ChunkSource`] over a runtime [`StreamReader`] — the consumer side of
/// the `response_forwarder` pump used by `putStream`. Yields buffered chunks,
/// blocks (waker-based) when the buffer is empty but the producer is still
/// live, errors on backpressure overflow, and ends at producer EOF.
///
/// Backpressure (the reason a large upload doesn't overflow): the forwarder's
/// V8 read loop PAUSES once the shared buffer crosses its high-water mark.
/// After draining a chunk here, if the buffer has fallen to the low-water
/// mark we ask the pump to resume the paused producer (`request_resume`), so
/// the upload proceeds in bounded-memory waves instead of racing the read
/// loop ahead of this S3-multipart consumer.
struct StreamReaderSource {
    reader: StreamReader,
    state: SharedState,
    stream_id: u32,
}

impl StreamReaderSource {
    /// Release backpressure if the buffer has drained enough: re-arm the
    /// paused producer. Cheap and idempotent — `request_resume` no-ops unless
    /// the forwarder is actually paused.
    fn maybe_resume_producer(&self) {
        if self.reader.buffered_bytes() <= response_forwarder::RESUME_LOW_WATER {
            response_forwarder::request_resume(&self.state, self.stream_id);
        }
    }
}

#[async_trait::async_trait(?Send)]
impl ChunkSource for StreamReaderSource {
    async fn next_chunk(&mut self) -> Option<ChunkResult> {
        loop {
            if let Some(chunk) = self.reader.pop() {
                // We just freed buffer space; let the producer refill it.
                self.maybe_resume_producer();
                return Some(Ok(bytes::Bytes::from(chunk)));
            }
            if self.reader.is_overflow() {
                return Some(Err(
                    "storage: upload stream exceeded the buffer backpressure cap".to_string(),
                ));
            }
            if self.reader.is_done() {
                return None;
            }
            // Buffer is empty and the producer may be paused (it pauses on
            // high-water, but a final short chunk can leave it paused with the
            // buffer already drained). Nudge a resume before parking so we
            // never deadlock waiting for data the paused producer won't send.
            self.maybe_resume_producer();
            self.reader.wait_for_data().await;
        }
    }
}

/// Promise plumbing for the `OpResult::JsValue` path (real JS values like a
/// `Uint8Array` chunk) — mirrors plugin-db's `setup_js_promise`.
fn setup_js_promise<'s>(
    scope: &mut v8::PinScope<'s, '_>,
    state: &SharedState,
) -> (
    v8::Global<v8::PromiseResolver>,
    Option<u64>,
    v8::Local<'s, v8::Promise>,
) {
    let resolver = v8::PromiseResolver::new(scope).unwrap();
    let promise = resolver.get_promise(scope);
    let global_resolver = v8::Global::new(scope, resolver);
    let request_id = state.borrow().executing_request_id;
    (global_resolver, request_id, promise)
}

fn require_u32_arg(
    scope: &mut v8::PinScope<'_, '_>,
    args: &v8::FunctionCallbackArguments,
    index: i32,
    name: &str,
) -> Option<u32> {
    let val = if args.length() > index {
        args.get(index)
    } else {
        return throw_type_u32(scope, name);
    };
    match val.uint32_value(scope) {
        Some(n) => Some(n),
        None => throw_type_u32(scope, name),
    }
}

fn throw_type_u32(scope: &mut v8::PinScope, arg_name: &str) -> Option<u32> {
    let msg = v8::String::new(scope, &format!("storage: argument '{arg_name}' must be a number"))
        .unwrap();
    let exc = v8::Exception::type_error(scope, msg);
    scope.throw_exception(exc);
    None
}

// ---------------------------------------------------------------------------
// putStream(bucket, key, readableStream, contentType?) → { bucket, key, size }
// ---------------------------------------------------------------------------

pub fn put_stream(
    scope: &mut v8::PinScope,
    args: v8::FunctionCallbackArguments,
    mut rv: v8::ReturnValue,
) {
    let state: SharedState = scope.get_slot::<SharedState>().expect("state").clone();

    let Some(bucket) = require_string_arg(scope, &args, 0, "bucket") else { return };
    let Some(key) = require_string_arg(scope, &args, 1, "key") else { return };

    // Arg 2 must be a ReadableStream (any object with the reader surface).
    let stream_v = if args.length() > 2 {
        args.get(2)
    } else {
        let _ = throw_type(scope, "stream");
        return;
    };
    let Ok(stream_obj) = v8::Local::<v8::Object>::try_from(stream_v) else {
        let _ = throw_type(scope, "stream");
        return;
    };
    let content_type = optional_string_arg(scope, &args, 3);

    let app_id = get_app_id(&state);
    let (op_id, request_id, promise) = setup_promise(scope, &state);

    let backend = match current_backend() {
        Ok(r) => r,
        Err(e) => {
            state.borrow_mut().spawned_ops.push(Box::pin(async move {
                OpResult::Failed { op_id, error: e, request_id }
            }));
            rv.set(promise.into());
            return;
        }
    };

    // Lock the app's ReadableStream and start the read-loop pump. Chunks
    // flow into `writer`; the spawned op drains `reader`.
    let stream_id = match response_forwarder::begin_forward_stream(scope, stream_obj) {
        Ok(id) => id,
        Err(e) => {
            state.borrow_mut().spawned_ops.push(Box::pin(async move {
                OpResult::Failed {
                    op_id,
                    error: format!("storage: put stream: {e}"),
                    request_id,
                }
            }));
            rv.set(promise.into());
            return;
        }
    };
    let (writer, reader) = stream_buffer();
    response_forwarder::attach_writer(&state, stream_id, writer);

    let source = StreamReaderSource { reader, state: state.clone(), stream_id };
    state.borrow_mut().spawned_ops.push(Box::pin(async move {
        match backend
            .put_stream(&app_id, &bucket, &key, Box::new(source), content_type.as_deref())
            .await
        {
            Ok(size) => OpResult::Completed {
                op_id,
                value: json!({ "bucket": bucket, "key": key, "size": size }).to_string(),
                request_id,
            },
            Err(e) => OpResult::Failed { op_id, error: e, request_id },
        }
    }));

    rv.set(promise.into());
}

// ---------------------------------------------------------------------------
// getStream(bucket, key) → { streamId, contentType, size } | null
// ---------------------------------------------------------------------------

pub fn get_stream(
    scope: &mut v8::PinScope,
    args: v8::FunctionCallbackArguments,
    mut rv: v8::ReturnValue,
) {
    let state: SharedState = scope.get_slot::<SharedState>().expect("state").clone();

    let Some(bucket) = require_string_arg(scope, &args, 0, "bucket") else { return };
    let Some(key) = require_string_arg(scope, &args, 1, "key") else { return };

    let app_id = get_app_id(&state);
    let (op_id, request_id, promise) = setup_promise(scope, &state);

    let backend = match current_backend() {
        Ok(r) => r,
        Err(e) => {
            state.borrow_mut().spawned_ops.push(Box::pin(async move {
                OpResult::Failed { op_id, error: e, request_id }
            }));
            rv.set(promise.into());
            return;
        }
    };

    state.borrow_mut().spawned_ops.push(Box::pin(async move {
        match backend.get_stream(&app_id, &bucket, &key).await {
            Ok(None) => OpResult::Completed { op_id, value: "null".into(), request_id },
            Ok(Some((meta, source))) => {
                let stream_id = alloc_get_stream_id();
                GET_STREAMS.with(|m| {
                    m.borrow_mut()
                        .insert(stream_id, Rc::new(RefCell::new(Some(source))));
                });
                OpResult::Completed {
                    op_id,
                    value: get_stream_handle_json(stream_id, &meta),
                    request_id,
                }
            }
            Err(e) => OpResult::Failed { op_id, error: e, request_id },
        }
    }));

    rv.set(promise.into());
}

fn get_stream_handle_json(stream_id: u32, meta: &ObjectMeta) -> String {
    json!({
        "streamId": stream_id,
        "contentType": meta.content_type,
        "size": meta.size,
    })
    .to_string()
}

// ---------------------------------------------------------------------------
// readChunk(streamId) → Uint8Array | undefined  (undefined = EOF)
// ---------------------------------------------------------------------------

pub fn read_chunk(
    scope: &mut v8::PinScope,
    args: v8::FunctionCallbackArguments,
    mut rv: v8::ReturnValue,
) {
    let state: SharedState = scope.get_slot::<SharedState>().expect("state").clone();

    let Some(stream_id) = require_u32_arg(scope, &args, 0, "streamId") else { return };
    let (resolver, request_id, promise) = setup_js_promise(scope, &state);

    let slot = GET_STREAMS.with(|m| m.borrow().get(&stream_id).cloned());
    let Some(slot) = slot else {
        // Unknown / already-finished stream → resolve EOF (undefined) so the
        // SDK's pull loop closes cleanly rather than rejecting.
        state.borrow_mut().spawned_ops.push(Box::pin(async move {
            OpResult::JsValue { resolver, value: ResolveValue::Undefined, request_id }
        }));
        rv.set(promise.into());
        return;
    };

    state.borrow_mut().spawned_ops.push(Box::pin(async move {
        // Take the source out for the pull, then put it back. compio is
        // single-threaded and the SDK pulls sequentially, so no two
        // `readChunk`s for the same id overlap.
        let mut source = match slot.borrow_mut().take() {
            Some(s) => s,
            None => {
                return OpResult::JsValue {
                    resolver,
                    value: ResolveValue::Undefined,
                    request_id,
                };
            }
        };
        let next = source.next_chunk().await;
        match next {
            Some(Ok(chunk)) => {
                *slot.borrow_mut() = Some(source);
                OpResult::JsValue {
                    resolver,
                    value: ResolveValue::Bytes(chunk.to_vec()),
                    request_id,
                }
            }
            Some(Err(e)) => {
                drop_get_stream(stream_id);
                OpResult::JsValue {
                    resolver,
                    value: ResolveValue::RejectError(OpError::error(e)),
                    request_id,
                }
            }
            None => {
                drop_get_stream(stream_id);
                OpResult::JsValue { resolver, value: ResolveValue::Undefined, request_id }
            }
        }
    }));

    rv.set(promise.into());
}

// ---------------------------------------------------------------------------
// cancelStream(streamId) → undefined
// ---------------------------------------------------------------------------

pub fn cancel_stream(
    scope: &mut v8::PinScope,
    args: v8::FunctionCallbackArguments,
    mut rv: v8::ReturnValue,
) {
    let state: SharedState = scope.get_slot::<SharedState>().expect("state").clone();
    let Some(stream_id) = require_u32_arg(scope, &args, 0, "streamId") else { return };
    let (resolver, request_id, promise) = setup_js_promise(scope, &state);
    drop_get_stream(stream_id);
    state.borrow_mut().spawned_ops.push(Box::pin(async move {
        OpResult::JsValue { resolver, value: ResolveValue::Undefined, request_id }
    }));
    rv.set(promise.into());
}
