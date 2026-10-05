//! V8 callbacks for `zeroship.storage.*` methods.
//!
//! Each callback mirrors the plugin-db pattern: parse args, allocate a
//! promise, push an async op into the runtime pump's spawned-ops queue,
//! return the promise. The pump resolves/rejects via OpResult.

use base64::Engine;
use serde_json::json;
use zeroship_runtime::state::{NativeValue, OpError, OpResult, ResolveValue, SharedState};
use zeroship_runtime::streams::stream_forwarder::{self, ChunkPolicy, UploadOptions, UploadReader};

use zeroship_storage::backend::{ChunkResult, ChunkSource, ObjectMeta};
use zeroship_storage::StorageError;
use crate::download::{Download, Gathered};
use crate::StorageContext;

/// Raw usage metrics a storage op emits in its success arm. `storage_ops`
/// counts every successful object op; `storage_bytes` accumulates bytes
/// WRITTEN (put), `storage_egress_bytes` bytes READ (get). Platform-
/// measured — emitted by trusted Rust inside the primitive, not by app
/// code. None are fixed platform counters, so they flow through
/// `AppUsage.custom`.
const STORAGE_OPS: &str = "storage_ops";
const STORAGE_BYTES: &str = "storage_bytes";
const STORAGE_EGRESS_BYTES: &str = "storage_egress_bytes";


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

/// Read a string property off an options object. Absent object, absent
/// property, `null`/`undefined`, or an empty string all read as `None`.
fn opt_string_field(
    scope: &mut v8::PinScope<'_, '_>,
    opts: Option<v8::Local<v8::Value>>,
    name: &str,
) -> Option<String> {
    let val = opt_field(scope, opts, name)?;
    let s = val.to_rust_string_lossy(scope);
    if s.is_empty() { None } else { Some(s) }
}

/// Read a numeric property off an options object. A non-numeric value reads
/// as `None`; callers normalise (e.g. `limits::resolve_list_limit` clamps).
fn opt_number_field(
    scope: &mut v8::PinScope<'_, '_>,
    opts: Option<v8::Local<v8::Value>>,
    name: &str,
) -> Option<f64> {
    opt_field(scope, opts, name)?.number_value(scope)
}

fn opt_field<'s>(
    scope: &mut v8::PinScope<'s, '_>,
    opts: Option<v8::Local<v8::Value>>,
    name: &str,
) -> Option<v8::Local<'s, v8::Value>> {
    let obj = opts?.to_object(scope)?;
    let key = v8::String::new(scope, name)?;
    let val = obj.get(scope, key.into())?;
    if val.is_null_or_undefined() { None } else { Some(val) }
}

fn throw_type(scope: &mut v8::PinScope, arg_name: &str) -> Option<String> {
    let msg = v8::String::new(scope, &format!("storage: missing required argument '{arg_name}'")).unwrap();
    let exc = v8::Exception::type_error(scope, msg);
    scope.throw_exception(exc);
    None
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

fn current_context(scope: &mut v8::PinScope<'_, '_>) -> Option<StorageContext> {
    if let Some(context) = scope.get_slot::<StorageContext>() {
        return Some(context.clone());
    }
    let message = v8::String::new(scope, "storage: no app storage handle was bound")?;
    let exception = v8::Exception::type_error(scope, message);
    scope.throw_exception(exception);
    None
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
    let Some(context) = current_context(scope) else { return };

    let Some(bucket) = require_string_arg(scope, &args, 0, "bucket") else { return };
    let Some(key) = require_string_arg(scope, &args, 1, "key") else { return };
    if args.length() <= 2 || args.get(2).is_null_or_undefined() {
        let _ = throw_type(scope, "bytesBase64");
        return;
    }
    let b64 = args.get(2).to_rust_string_lossy(scope);
    let content_type = optional_string_arg(scope, &args, 3);

    let (op_id, request_id, promise) = setup_promise(scope, &state);

    let max_encoded = context.storage.max_buffered_bytes().div_ceil(3).saturating_mul(4);
    if b64.len() as u64 > max_encoded {
        state.borrow_mut().spawned_ops.push(Box::pin(async move {
            OpResult::Failed {
                op_id,
                error: "storage: buffered object exceeds size limit; use putStream".into(),
                request_id,
            }
        }));
        rv.set(promise.into());
        return;
    }

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


    let meter = context.meter.clone();
    state.borrow_mut().spawned_ops.push(Box::pin(async move {
        match context.storage.put(&bucket, &key, &bytes, content_type.as_deref()).await {
            Ok(size) => {
                // Success arm only: one op + bytes written. Unforgeable.
                if let Some(m) = &meter {
                    m.record(STORAGE_OPS, 1);
                    m.record(STORAGE_BYTES, size);
                }
                OpResult::Completed {
                    op_id,
                    value: json!({ "bucket": bucket, "key": key, "size": size }).to_string(),
                    request_id,
                }
            }
            Err(e) => OpResult::Failed { op_id, error: e.to_string(), request_id },
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
    let Some(context) = current_context(scope) else { return };

    let Some(bucket) = require_string_arg(scope, &args, 0, "bucket") else { return };
    let Some(key) = require_string_arg(scope, &args, 1, "key") else { return };

    let (op_id, request_id, promise) = setup_promise(scope, &state);


    let meter = context.meter.clone();
    state.borrow_mut().spawned_ops.push(Box::pin(async move {
        match context.storage.get(&bucket, &key).await {
            Ok(None) => {
                // A miss is still a successful read op (one storage op,
                // zero egress bytes).
                if let Some(m) = &meter {
                    m.record(STORAGE_OPS, 1);
                }
                OpResult::Completed {
                    op_id,
                    value: "null".into(),
                    request_id,
                }
            }
            Ok(Some((bytes, meta))) => {
                // Success arm only: one op + bytes read (egress). Unforgeable.
                if let Some(m) = &meter {
                    m.record(STORAGE_OPS, 1);
                    m.record(STORAGE_EGRESS_BYTES, meta.size);
                }
                let b64 = base64::engine::general_purpose::STANDARD.encode(&bytes);
                let out = json!({
                    "bytesBase64": b64,
                    "contentType": meta.content_type,
                    "size": meta.size,
                });
                OpResult::Completed { op_id, value: out.to_string(), request_id }
            }
            Err(e) => OpResult::Failed { op_id, error: e.to_string(), request_id },
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
    let Some(context) = current_context(scope) else { return };

    let Some(bucket) = require_string_arg(scope, &args, 0, "bucket") else { return };
    let Some(key) = require_string_arg(scope, &args, 1, "key") else { return };

    let (op_id, request_id, promise) = setup_promise(scope, &state);


    let meter = context.meter.clone();
    state.borrow_mut().spawned_ops.push(Box::pin(async move {
        match context.storage.delete(&bucket, &key).await {
            Ok(deleted) => {
                // Success arm only: one storage op (whether or not a key
                // existed — the delete itself ran).
                if let Some(m) = &meter {
                    m.record(STORAGE_OPS, 1);
                }
                OpResult::Completed {
                    op_id,
                    value: json!({ "deleted": deleted }).to_string(),
                    request_id,
                }
            }
            Err(e) => OpResult::Failed { op_id, error: e.to_string(), request_id },
        }
    }));

    rv.set(promise.into());
}

// ---------------------------------------------------------------------------
// list(bucket, prefix?, { cursor?, limit? })
//   → { entries: [{ key, size, modifiedAt }], cursor: string | null }
// ---------------------------------------------------------------------------

pub fn list(
    scope: &mut v8::PinScope,
    args: v8::FunctionCallbackArguments,
    mut rv: v8::ReturnValue,
) {
    let state: SharedState = scope.get_slot::<SharedState>().expect("state").clone();
    let Some(context) = current_context(scope) else { return };

    let Some(bucket) = require_string_arg(scope, &args, 0, "bucket") else { return };
    let prefix = optional_string_arg(scope, &args, 1).unwrap_or_default();
    let opts = if args.length() > 2 { Some(args.get(2)) } else { None };
    let cursor = opt_string_field(scope, opts, "cursor");
    let limit = zeroship_storage::limits::resolve_list_limit(opt_number_field(scope, opts, "limit"));

    let (op_id, request_id, promise) = setup_promise(scope, &state);


    let meter = context.meter.clone();
    state.borrow_mut().spawned_ops.push(Box::pin(async move {
        let req = zeroship_storage::backend::ListRequest {
            prefix: &prefix,
            cursor: cursor.as_deref(),
            limit,
        };
        match context.storage.list(&bucket, req).await {
            Ok(page) => {
                // Success arm only: one storage op (the list).
                if let Some(m) = &meter {
                    m.record(STORAGE_OPS, 1);
                }
                let arr: Vec<serde_json::Value> = page.entries.into_iter().map(|e| {
                    let modified = e.modified_at
                        .duration_since(std::time::UNIX_EPOCH)
                        .map(|d| d.as_millis() as u64)
                        .unwrap_or(0);
                    json!({ "key": e.key, "size": e.size, "modifiedAt": modified })
                }).collect();
                // `cursor` is the truncation signal: null iff the listing is
                // complete. Never omit it — a caller that cannot tell a full
                // answer from a partial one is the bug this shape exists to
                // prevent.
                let cursor_v = match page.cursor {
                    Some(c) => serde_json::Value::String(c),
                    None => serde_json::Value::Null,
                };
                OpResult::Completed {
                    op_id,
                    value: json!({ "entries": arr, "cursor": cursor_v }).to_string(),
                    request_id,
                }
            }
            Err(e) => OpResult::Failed { op_id, error: e.to_string(), request_id },
        }
    }));

    rv.set(promise.into());
}

// Uploads bridge V8 to the Rust chunk source. Downloads remain owned by
// the context captured from this isolate, including across asynchronous pulls.

/// A [`ChunkSource`] over the runtime's [`UploadReader`], the consumer side
/// of the `stream_forwarder` upload that `putStream` starts. The reader
/// carries the backpressure (the V8 read loop pauses at the channel's
/// high-water mark and this consumer's draining re-arms it), so a large
/// upload proceeds in bounded-memory waves against the S3-multipart
/// consumer.
struct UploadSource(UploadReader);

#[async_trait::async_trait(?Send)]
impl ChunkSource for UploadSource {
    async fn next_chunk(&mut self) -> Option<ChunkResult> {
        match self.0.next_chunk().await? {
            Ok(chunk) => Some(Ok(chunk)),
            // A source that failed partway is an error, never a complete
            // object: committing it would store a prefix as if it were whole.
            Err(err) => Some(Err(StorageError::Stream(format!("storage: {err}")))),
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
    let Some(context) = current_context(scope) else { return };

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

    let (op_id, request_id, promise) = setup_promise(scope, &state);


    // Lock the app's ReadableStream and start the read-loop pump into a
    // dedicated upload channel sized to 2x the S3 part size, so the producer
    // can fill the NEXT part while the current part PUTs (overlapping V8
    // chunk generation with the in-flight upload). The spawned op drains it.
    let options = UploadOptions {
        buffer_cap: crate::limits::UPLOAD_STREAM_BUFFER_CAP,
        chunk_policy: ChunkPolicy::Coerce,
        abort_signal: None,
        owner_request: request_id,
    };
    let upload = match stream_forwarder::forward_upload(scope, stream_obj, options) {
        Ok(upload) => upload,
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
    let source = UploadSource(upload);
    let meter = context.meter.clone();
    state.borrow_mut().spawned_ops.push(Box::pin(async move {
        match context.storage
            .put_stream(&bucket, &key, Box::new(source), content_type.as_deref())
            .await
        {
            Ok(size) => {
                // Success arm only: one op + the final streamed byte count.
                if let Some(m) = &meter {
                    m.record(STORAGE_OPS, 1);
                    m.record(STORAGE_BYTES, size);
                }
                OpResult::Completed {
                    op_id,
                    value: json!({ "bucket": bucket, "key": key, "size": size }).to_string(),
                    request_id,
                }
            }
            Err(e) => OpResult::Failed { op_id, error: e.to_string(), request_id },
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
    let Some(context) = current_context(scope) else { return };

    let Some(bucket) = require_string_arg(scope, &args, 0, "bucket") else { return };
    let Some(key) = require_string_arg(scope, &args, 1, "key") else { return };

    let (op_id, request_id, promise) = setup_promise(scope, &state);


    let meter = context.meter.clone();
    state.borrow_mut().spawned_ops.push(Box::pin(async move {
        match context.storage.get_stream(&bucket, &key).await {
            Ok(None) => {
                // A miss is a successful read op (zero egress).
                if let Some(m) = &meter {
                    m.record(STORAGE_OPS, 1);
                }
                OpResult::Completed { op_id, value: "null".into(), request_id }
            }
            Ok(Some((meta, source))) => {
                // Park the source in this isolate. A refusal here means
                // the app is over its live-stream cap; the source is dropped
                // (releasing its fd / HTTP body) and the op fails.
                let stream_id = match context.streams.open(Download::new(source, meta.size)) {
                    Ok(id) => id,
                    Err(e) => return OpResult::Failed { op_id, error: e.to_string(), request_id },
                };
                // Success arm only, and only once the handle is actually
                // live: one op + the object's full byte count as egress.
                // `meta.size` is the authoritative object size known at open;
                // the per-chunk reads (readChunk) are the transport of those
                // same bytes, so billing once here avoids double-counting.
                if let Some(m) = &meter {
                    m.record(STORAGE_OPS, 1);
                    m.record(STORAGE_EGRESS_BYTES, meta.size);
                }
                OpResult::Completed {
                    op_id,
                    value: get_stream_handle_json(stream_id, &meta),
                    request_id,
                }
            }
            Err(e) => OpResult::Failed { op_id, error: e.to_string(), request_id },
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
    let Some(context) = current_context(scope) else { return };

    let Some(stream_id) = require_u32_arg(scope, &args, 0, "streamId") else { return };
    // The server-injected APP_ID, not anything the app can choose. A stream
    // owned by a co-resident app is simply not found.
    let (resolver, request_id, promise) = setup_js_promise(scope, &state);

    let slot = context.streams.slot(stream_id);
    let Some(slot) = slot else {
        // Unknown / already-finished / not-ours stream → resolve EOF
        // (undefined) so the SDK's pull loop closes cleanly rather than
        // rejecting. A stream belonging to another app is deliberately
        // indistinguishable from one that never existed.
        state.borrow_mut().spawned_ops.push(Box::pin(async move {
            OpResult::JsValue { resolver, value: ResolveValue::Undefined, request_id }
        }));
        rv.set(promise.into());
        return;
    };

    state.borrow_mut().spawned_ops.push(Box::pin(async move {
        // Take the download out for the pull, then put it back. compio is
        // single-threaded and the SDK pulls sequentially, so no two
        // `readChunk`s for the same id overlap.
        let Some(mut download) = slot.borrow_mut().download.take() else {
            return OpResult::JsValue { resolver, value: ResolveValue::Undefined, request_id };
        };
        let gathered = download
            .gather(
                crate::limits::DOWNLOAD_CHUNK_BYTES,
                crate::limits::DOWNLOAD_GATHER_BUDGET,
                || context.streams.is_open(stream_id, &slot),
            )
            .await;
        let value = match gathered {
            Gathered::Chunk(chunk) => {
                slot.borrow_mut().download = Some(download);
                ResolveValue::Native(Box::new(ChunkValue(chunk)))
            }
            // The object's bytes end here. Closing now releases the backend
            // body and the permit; the next `readChunk` finds no handle and
            // resolves EOF.
            Gathered::Last(chunk) => {
                context.streams.close_slot(stream_id, &slot);
                if chunk.is_empty() {
                    ResolveValue::Undefined
                } else {
                    ResolveValue::Native(Box::new(ChunkValue(chunk)))
                }
            }
            Gathered::Failed(e) => {
                context.streams.close_slot(stream_id, &slot);
                ResolveValue::RejectError(OpError::error(e.to_string()))
            }
            // Cancelled before the gather ended, whatever it gathered: the read
            // resolves EOF and the download drops here with its source.
            Gathered::Closed => ResolveValue::Undefined,
        };
        OpResult::JsValue { resolver, value, request_id }
    }));

    rv.set(promise.into());
}

/// A gathered chunk, handed to V8 as the backing store of a fresh
/// `Uint8Array` so its bytes are not copied again.
struct ChunkValue(Vec<u8>);

impl NativeValue for ChunkValue {
    fn into_v8<'s>(
        self: Box<Self>,
        scope: &mut v8::PinScope<'s, '_>,
    ) -> Result<v8::Local<'s, v8::Value>, OpError> {
        let length = self.0.len();
        let store = v8::ArrayBuffer::new_backing_store_from_vec(self.0).make_shared();
        let buffer = v8::ArrayBuffer::with_backing_store(scope, &store);
        v8::Uint8Array::new(scope, buffer, 0, length)
            .map(Into::into)
            .ok_or_else(|| OpError::error("storage: could not allocate a download chunk"))
    }
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
    let Some(context) = current_context(scope) else { return };
    let Some(stream_id) = require_u32_arg(scope, &args, 0, "streamId") else { return };
    // Scoped to the caller's own app: cancelling a co-resident app's stream
    // is a no-op, not a reclaim.
    let (resolver, request_id, promise) = setup_js_promise(scope, &state);
    context.streams.close(stream_id);
    state.borrow_mut().spawned_ops.push(Box::pin(async move {
        OpResult::JsValue { resolver, value: ResolveValue::Undefined, request_id }
    }));
    rv.set(promise.into());
}
