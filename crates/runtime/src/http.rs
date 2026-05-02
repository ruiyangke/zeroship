//! HTTP dispatch primitives — pure V8 logic for inspecting Response objects.
//!
//! Extracted from `runtime-tokio` so both `runtime-tokio` and `runtime-compio`
//! can reuse the same V8 property-access and response-inspection code.

use crate::state::SharedState;

// ---------------------------------------------------------------------------
// Hot property-name keys — statically backed v8 strings
// ---------------------------------------------------------------------------
//
// `v8::String::new(scope, "literal")` does a UTF-8 validity check, a heap
// allocation, and an intern-pool hash lookup every time it's called. For the
// property names read on every request we instead use `OneByteConst` — a
// static C++-side string resource that's compiled into the binary. V8 only
// holds a pointer to it, so the per-call cost drops to a single pointer
// load. Measurable on the hot WinterCG fetch path where inspect_response
// runs the same ~8 property accesses per response.
static K_STATUS: v8::OneByteConst = v8::String::create_external_onebyte_const(b"status");
static K_HEADERS: v8::OneByteConst = v8::String::create_external_onebyte_const(b"headers");
static K_ID: v8::OneByteConst = v8::String::create_external_onebyte_const(b"_id");
static K_MESSAGE: v8::OneByteConst = v8::String::create_external_onebyte_const(b"message");
static K_DONE: v8::OneByteConst = v8::String::create_external_onebyte_const(b"done");
static K_VALUE: v8::OneByteConst = v8::String::create_external_onebyte_const(b"value");
static K_NEXT: v8::OneByteConst = v8::String::create_external_onebyte_const(b"next");

#[inline(always)]
fn key<'s>(scope: &mut v8::PinScope<'s, '_>, k: &'static v8::OneByteConst) -> v8::Local<'s, v8::String> {
    v8::String::new_from_onebyte_const(scope, k).unwrap()
}

// ---------------------------------------------------------------------------
// HTTP helper constants
// ---------------------------------------------------------------------------

/// JS helper compiled once: constructs a `Request` from Rust-supplied params.
///
/// Goes through the public spec constructor `new Request(url, init)`. The
/// previous shape-direct path (`Object.create(Request.prototype) + obj.set
/// per field`) only worked against the JS polyfill — the native Request
/// (`#[v8_class]` with internal field 0 holding `Box<RequestState>`) rejects
/// `Object.create(prototype)` because the resulting instance has a null
/// internal field, and the first getter call throws "Illegal invocation".
///
/// Per D-22, the JSON header marshalling is retired with the polyfill in
/// landing 2: native `fetch()` builds Request objects directly inside V8,
/// no JSON intermediate. This helper remains for the kernel's slow-path
/// `default.fetch` dispatch where Rust passes raw header bytes; once the
/// dispatch path itself moves to native (post-D-23), this constant goes
/// away too.
///
/// Fast paths still preserved:
///   - Skips `JSON.parse` when `headersJson` is empty or `"[]"`.
///   - Skips body copy when body is empty or method is GET/HEAD.
pub const HTTP_CREATE_REQUEST_JS: &str = r#"(function(method, url, headersJson, body) {
    // "[]" is 2 chars; anything longer means at least one real header.
    var pairs;
    if (headersJson && headersJson.length > 2) {
        pairs = JSON.parse(headersJson);
    } else {
        pairs = [];
    }
    // Build init lazily — body is only included for methods that allow one.
    var init;
    if (body && method !== "GET" && method !== "HEAD") {
        init = { method: method, headers: pairs, body: body };
    } else {
        init = { method: method, headers: pairs };
    }
    return new Request(url, init);
})"#;

// ---------------------------------------------------------------------------
// V8 property access helpers
// ---------------------------------------------------------------------------

pub fn get_string_property(scope: &mut v8::PinScope, obj: v8::Local<v8::Object>, key: &str) -> String {
    let key = v8::String::new(scope, key).unwrap();
    obj.get(scope, key.into())
        .map(|v| v.to_rust_string_lossy(scope))
        .unwrap_or_default()
}

pub fn get_u32_property(scope: &mut v8::PinScope, obj: v8::Local<v8::Object>, key: &str) -> u32 {
    let key = v8::String::new(scope, key).unwrap();
    obj.get(scope, key.into())
        .and_then(|v| v.uint32_value(scope))
        .unwrap_or(0)
}

pub fn get_bool_property(scope: &mut v8::PinScope, obj: v8::Local<v8::Object>, key: &str) -> bool {
    let key = v8::String::new(scope, key).unwrap();
    obj.get(scope, key.into())
        .map(|v| v.boolean_value(scope))
        .unwrap_or(false)
}

// ---------------------------------------------------------------------------
// Response types
// ---------------------------------------------------------------------------

/// Classification of an inspected V8 Response object.
pub enum ResponseInfo {
    /// Complete buffered response.
    Complete {
        status: u16,
        headers: Vec<(String, String)>,
        body: String,
    },
    /// Streaming response — body arrives via the stream forwarder.
    Stream {
        status: u16,
        headers: Vec<(String, String)>,
        stream_id: u32,
    },
    /// WebSocket upgrade — JS returned `new Response(null, { status: 101, webSocket: client })`.
    WebSocket {
        ws_id: u32,
        headers: Vec<(String, String)>,
    },
}

/// Result of settling a pending request — RPC or HTTP path.
pub enum SettledResult {
    /// RPC path: JSON string result.
    Rpc(Result<String, String>),
    /// HTTP path: inspected Response object.
    Http(Result<ResponseInfo, String>),
}

// ---------------------------------------------------------------------------
// Response inspection
// ---------------------------------------------------------------------------

/// Inspect a V8 Response object and extract status, headers, body / stream info.
///
/// Per D-23 step 3 the polyfill is gone — every Response that reaches this
/// inspector is either a native `#[v8_class]` Response (Box<ResponseState>
/// in internal field 0 — see `crate::fetch_response`) or a duck-typed
/// `{ status, ... }` plain object the user returned from a handler. The
/// native case is the hot path; the duck-type case falls through to the
/// "wrap as plain text" branch below.
pub fn inspect_response(scope: &mut v8::PinScope, response_val: v8::Local<v8::Value>) -> Result<ResponseInfo, String> {
    let obj = match response_val.to_object(scope) {
        Some(o) => o,
        None => return Err("Handler did not return a Response object".to_string()),
    };

    let status_key = key(scope, &K_STATUS);
    let status = obj.get(scope, status_key.into())
        .and_then(|v| v.uint32_value(scope))
        .unwrap_or(0) as u16;
    if status == 0 {
        // Likely not a Response — wrap raw value as 200 text body
        let body = response_val.to_rust_string_lossy(scope);
        return Ok(ResponseInfo::Complete {
            status: 200,
            headers: vec![("content-type".into(), "text/plain;charset=UTF-8".into())],
            body,
        });
    }

    // Extract headers from response.headers (native Headers iterates via
    // the WebIDL `iterable<>` mixin).
    let headers = extract_response_headers(scope, obj);

    // WebSocket upgrade: status 101 with a `webSocket` property. The
    // gateway path stashes the client WebSocket Global on the Response;
    // we ferry the `ws_id` through to the kernel.
    //
    // Two layouts in flight during the cutover (D-25):
    //   - polyfill: `webSocket` is a plain JS object with an `_id`
    //     expando set by `__wsCreatePair` / `__wsLinkPair`. Read via
    //     `ws_obj.get("_id")`.
    //   - native (feature `runtime_native_websocket` ON): `webSocket`
    //     is a native `#[v8_class]` WebSocket whose `ws_id` lives in
    //     the boxed state (internal field 0). Read via
    //     `websocket_native::ws_id_of`.
    //
    // We try the native path first, then fall back to the polyfill
    // expando. (per design §X.1 — addresses the v1 design's "polyfill
    // `_id` field" gap.)
    if status == 101 {
        if let Some(ws_g) = crate::fetch_response::try_native_response_websocket(scope, obj) {
            let ws_obj = v8::Local::new(scope, ws_g);

            // Native path first: read ws_id from the boxed state.
            let mut ws_id = crate::websocket_native::ws_id_of(scope, ws_obj);

            // Fallback: polyfill `_id` expando.
            if ws_id == 0 {
                let id_key = key(scope, &K_ID);
                ws_id = ws_obj
                    .get(scope, id_key.into())
                    .and_then(|v| v.uint32_value(scope))
                    .unwrap_or(0);
            }

            return Ok(ResponseInfo::WebSocket { ws_id, headers });
        }
    }

    // Native Response: read body via the public surface (D-23). For
    // duck-typed `{ status, ... }` plain objects, fall through with an
    // empty body.
    if let Some(view) = crate::fetch_response::try_native_response_body(scope, obj) {
        return inspect_native_response(scope, obj, status, headers, view);
    }
    Ok(ResponseInfo::Complete { status, headers, body: String::new() })
}

/// Read a native Response body via the public surface (no polyfill probes).
fn inspect_native_response(
    scope: &mut v8::PinScope,
    obj: v8::Local<v8::Object>,
    status: u16,
    headers: Vec<(String, String)>,
    view: crate::fetch_response::NativeResponseBody,
) -> Result<ResponseInfo, String> {
    use crate::fetch_response::NativeResponseBody;
    match view {
        NativeResponseBody::Empty => Ok(ResponseInfo::Complete {
            status,
            headers,
            body: String::new(),
        }),
        NativeResponseBody::Bytes(rc) => {
            // Materialise as UTF-8 text. The wire path treats body as
            // a String; binary bodies survive lossy conversion because
            // the resulting Rust String is round-tripped to bytes when
            // sent on the wire.
            let body = String::from_utf8_lossy(&rc).into_owned();
            Ok(ResponseInfo::Complete { status, headers, body })
        }
        NativeResponseBody::Stream => {
            // Lock the body's ReadableStream and start the body pump.
            // The Rust-side forwarder (replaces the legacy JS pump in
            // `__zsBeginStreamForward`) reads `response.body`, calls
            // `getReader()` on it, and drives the read loop via Rust
            // promise reactions. Each chunk lands in a per-stream
            // forwarder (registered on SharedState by `stream_id`),
            // which buffers until the kernel attaches a `direct_writer`
            // in `build_fetch_outcome`.
            let stream_id = crate::streams::response_forwarder::begin_forward(scope, obj)?;
            classify_stream(scope, status, headers, stream_id)
        }
    }
}

/// After a stream id is allocated, decide whether the body is already
/// closed (sync-enqueued in start()) or still open (returns Stream so
/// chunks can flow async).
fn classify_stream(
    scope: &mut v8::PinScope,
    status: u16,
    headers: Vec<(String, String)>,
    stream_id: u32,
) -> Result<ResponseInfo, String> {
    let state: SharedState = scope
        .get_slot::<SharedState>()
        .expect("RuntimeState not in isolate slot")
        .clone();

    let stream_closed = crate::streams::response_forwarder::is_closed(&state, stream_id);

    if stream_closed {
        // Stream sync-completed in start() — collect buffered chunks
        // as a complete body and discard the forwarder.
        let chunks = crate::streams::response_forwarder::drain_into_complete(&state, stream_id);
        let body_text = chunks
            .iter()
            .map(|b| String::from_utf8_lossy(b).to_string())
            .collect::<String>();
        Ok(ResponseInfo::Complete { status, headers, body: body_text })
    } else {
        // Stream still open — return as streaming. The runtime will
        // attach a direct writer in build_fetch_outcome; chunks
        // already buffered in the forwarder will be drained then.
        Ok(ResponseInfo::Stream { status, headers, stream_id })
    }
}

/// Extract headers from a Response object.
///
/// Hot path: pull the native Headers state pointer directly (FIX D —
/// see `crate::headers::try_native_headers`). Falls back to the
/// WebIDL `iterable<>` protocol
/// (`Headers.prototype[Symbol.iterator]()` per WHATWG Fetch §2.2) for
/// any non-native Headers shape that user code may have substituted.
pub fn extract_response_headers(scope: &mut v8::PinScope, response_obj: v8::Local<v8::Object>) -> Vec<(String, String)> {
    let headers_key = key(scope, &K_HEADERS);
    let Some(headers_val) = response_obj.get(scope, headers_key.into()) else { return Vec::new() };
    let Some(headers_obj) = headers_val.to_object(scope) else { return Vec::new() };

    // FIX D fast path: native Headers state pointer access — skips
    // the WebIDL iterable<> protocol (one Function.call per pair,
    // step.done lookup, etc.). Note that for the WinterCG response
    // path we don't need spec-sorted headers — wire emission order
    // doesn't depend on iteration order, and the kernel forwards the
    // raw list to the HTTP writer. Going through the iterator would
    // sort+combine, which is observable to user code via `for ... of
    // headers` but wasted work for the wire path.
    if let Some(headers_state) = crate::headers::try_native_headers(scope, headers_obj) {
        let list = headers_state.list();
        let mut result: Vec<(String, String)> = Vec::with_capacity(list.len());
        for (n, v) in list {
            result.push((
                String::from_utf8_lossy(n).into_owned(),
                String::from_utf8_lossy(v).into_owned(),
            ));
        }
        return result;
    }

    // Slow path: a user-supplied non-native Headers shape — go through
    // the WebIDL iterable<> protocol.
    let mut result = Vec::new();

    let sym_iter = v8::Symbol::get_iterator(scope);
    let Some(iter_fn_val) = headers_obj.get(scope, sym_iter.into()) else { return result };
    let Ok(iter_fn) = v8::Local::<v8::Function>::try_from(iter_fn_val) else { return result };
    let Some(iter_v) = iter_fn.call(scope, headers_obj.into(), &[]) else { return result };
    let Some(iter_obj) = iter_v.to_object(scope) else { return result };

    let next_key = key(scope, &K_NEXT);
    let done_key = key(scope, &K_DONE);
    let value_key = key(scope, &K_VALUE);
    let Some(next_fn_val) = iter_obj.get(scope, next_key.into()) else { return result };
    let Ok(next_fn) = v8::Local::<v8::Function>::try_from(next_fn_val) else { return result };

    loop {
        let Some(step_v) = next_fn.call(scope, iter_obj.into(), &[]) else { break };
        let Some(step_obj) = step_v.to_object(scope) else { break };
        let done = step_obj.get(scope, done_key.into())
            .map(|v| v.boolean_value(scope))
            .unwrap_or(true);
        if done { break; }
        let Some(pair_val) = step_obj.get(scope, value_key.into()) else { break };
        let Some(pair_obj) = pair_val.to_object(scope) else { continue };
        let Some(name_val) = pair_obj.get_index(scope, 0) else { continue };
        let Some(val_val) = pair_obj.get_index(scope, 1) else { continue };
        result.push((
            name_val.to_rust_string_lossy(scope),
            val_val.to_rust_string_lossy(scope),
        ));
    }
    result
}

/// Fast test for a V8 value that should flow through `inspect_response`
/// instead of the JSON-wrap path.
///
/// Native Response: detected via internal-field 0 holding a non-null
/// Box<ResponseState> (the macro-emitted brand). Plain handler returns
/// (`{ status, url }`, primitives, arrays) don't carry the brand.
///
/// One internal-field probe per async RPC settlement. V8's inline cache
/// turns the lookup into a hidden-class check after warmup.
pub fn looks_like_response(scope: &mut v8::PinScope, val: v8::Local<v8::Value>) -> bool {
    // Fast reject: primitives and null can't carry brands.
    if !val.is_object() { return false; }
    let Some(obj) = val.to_object(scope) else { return false; };
    crate::fetch_response::is_native_response(scope, obj)
}

/// Extract the result of a settled promise. All dispatch goes through
/// `call_fetch_handler` now, so the fulfilled value is always inspected
/// as an HTTP `Response`.
pub fn extract_settled_result(
    scope: &mut v8::PinScope,
    promise: &v8::Global<v8::Promise>,
) -> SettledResult {
    let local = v8::Local::new(scope, promise);
    match local.state() {
        v8::PromiseState::Fulfilled => {
            let val = local.result(scope);
            SettledResult::Http(inspect_response(scope, val))
        }
        v8::PromiseState::Rejected => {
            // Read `.message` if it's an Error object; else stringify.
            let exc = local.result(scope);
            let msg = if let Some(obj) = exc.to_object(scope) {
                let msg_key = key(scope, &K_MESSAGE);
                obj.get(scope, msg_key.into())
                    .filter(|v| !v.is_undefined() && !v.is_null())
                    .and_then(|v| v.to_string(scope))
                    .map(|s| s.to_rust_string_lossy(scope))
                    .unwrap_or_else(|| exc.to_string(scope)
                        .map(|s| s.to_rust_string_lossy(scope))
                        .unwrap_or_else(|| "Promise rejected".to_string()))
            } else {
                exc.to_string(scope)
                    .map(|s| s.to_rust_string_lossy(scope))
                    .unwrap_or_else(|| "Promise rejected".to_string())
            };
            SettledResult::Http(Err(msg))
        }
        v8::PromiseState::Pending => {
            SettledResult::Http(Err("Promise still pending".to_string()))
        }
    }
}
