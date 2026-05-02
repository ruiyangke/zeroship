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
static K_BODY_TEXT: v8::OneByteConst = v8::String::create_external_onebyte_const(b"_bodyText");
static K_IS_STREAM: v8::OneByteConst = v8::String::create_external_onebyte_const(b"_isStreamBody");
static K_WEBSOCKET: v8::OneByteConst = v8::String::create_external_onebyte_const(b"webSocket");
static K_ID: v8::OneByteConst = v8::String::create_external_onebyte_const(b"_id");
static K_STREAM_ID: v8::OneByteConst = v8::String::create_external_onebyte_const(b"_streamId");
static K_LENGTH: v8::OneByteConst = v8::String::create_external_onebyte_const(b"length");
static K_ZS_RESPONSE: v8::OneByteConst = v8::String::create_external_onebyte_const(b"__zsResponse");
static K_MESSAGE: v8::OneByteConst = v8::String::create_external_onebyte_const(b"message");
static K_ZS_HEADERS_ARR: v8::OneByteConst = v8::String::create_external_onebyte_const(b"_zsHeadersArr");
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
/// Native vs polyfill: when the value is a native Response (Box<ResponseState>
/// in internal field 0 — see `crate::fetch_response`), the kernel reads body
/// state through the public `try_native_response_body` surface. Otherwise it
/// falls back to the polyfill's underscore-prefixed expandos (`_isStreamBody`,
/// `_streamId`, `_bodyText`). The polyfill probe goes away in D-23 landing 3
/// when `embed/fetch.js` is deleted.
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

    // Extract headers from response.headers (native or polyfill)
    let headers = extract_response_headers(scope, obj);

    // WebSocket upgrade: status 101 with a `webSocket` property. The
    // gateway path stashes the client WebSocket Global on the Response;
    // we ferry the `id` (a `#[v8_class]` field) through to the kernel.
    if status == 101 {
        // Native Response: read webSocket via the typed accessor.
        if let Some(ws_g) = crate::fetch_response::try_native_response_websocket(scope, obj) {
            let ws_obj = v8::Local::new(scope, ws_g);
            let id_key = key(scope, &K_ID);
            let ws_id = ws_obj.get(scope, id_key.into())
                .and_then(|v| v.uint32_value(scope))
                .unwrap_or(0);
            return Ok(ResponseInfo::WebSocket { ws_id, headers });
        }
        // Polyfill fallback (deleted in D-23 landing 3).
        let ws_key = key(scope, &K_WEBSOCKET);
        if let Some(ws_val) = obj.get(scope, ws_key.into()) {
            if !ws_val.is_undefined() && !ws_val.is_null() {
                if let Some(ws_obj) = ws_val.to_object(scope) {
                    let id_key = key(scope, &K_ID);
                    let ws_id = ws_obj.get(scope, id_key.into())
                        .and_then(|v| v.uint32_value(scope))
                        .unwrap_or(0);
                    return Ok(ResponseInfo::WebSocket { ws_id, headers });
                }
            }
        }
    }

    // Native Response fast path. When the wrapper has a Box<ResponseState>
    // in internal field 0, read the body through the public surface — no
    // reach into either impl's privates. The polyfill path below is the
    // fallback for `embed/fetch.js`-constructed Responses (deleted in D-23
    // landing 3 when the polyfill is gone).
    if let Some(view) = crate::fetch_response::try_native_response_body(scope, obj) {
        return inspect_native_response(scope, obj, status, headers, view);
    }

    // ---- Polyfill path (deleted with embed/fetch.js in landing 3) ----
    let is_stream_key = key(scope, &K_IS_STREAM);
    let is_stream = obj.get(scope, is_stream_key.into())
        .map(|v| v.boolean_value(scope))
        .unwrap_or(false);
    if is_stream {
        // Stream ID is on the Response itself: `response._streamId`.
        //
        // - If `_streamId >= 0`: already allocated (e.g. fetch-response body
        //   that wraps a Rust-pushed stream_id, or a Response inspected twice).
        // - If `_streamId === -1` (sentinel): the Response holds an
        //   un-pumped ReadableStream. Call `__zsBeginStreamForward(response)`
        //   to lock the body, allocate a streamId, and launch the pump.
        //
        // Reading `response._streamId` rather than `response.body._id`
        // keeps the kernel free of any reach into stream-class internals.
        // The forward helper duck-types on `getReader()` so it works
        // against native ReadableStream, polyfill streams, and any
        // spec-conformant class.
        let stream_id_key = key(scope, &K_STREAM_ID);
        let raw = obj.get(scope, stream_id_key.into());
        let needs_pump = raw.map(|v| v.int32_value(scope).unwrap_or(0) < 0).unwrap_or(true);
        let stream_id: u32 = if needs_pump {
            forward_stream(scope, obj)?
        } else {
            raw.and_then(|v| v.uint32_value(scope)).unwrap_or(0)
        };
        return classify_stream(scope, status, headers, stream_id);
    }
    let body_key = key(scope, &K_BODY_TEXT);
    let body = obj.get(scope, body_key.into())
        .map(|v| v.to_rust_string_lossy(scope))
        .unwrap_or_default();
    Ok(ResponseInfo::Complete { status, headers, body })
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
            // a String (matching the polyfill's _bodyText semantics);
            // binary bodies survive lossy conversion because the
            // resulting Rust String is round-tripped to bytes when
            // sent on the wire.
            let body = String::from_utf8_lossy(&rc).into_owned();
            Ok(ResponseInfo::Complete { status, headers, body })
        }
        NativeResponseBody::Stream => {
            // Lock the native ReadableStream on the Response and start
            // the body pump. `__zsBeginStreamForward` reads `response.body`
            // (native getter) and `response._streamId` (allowed expando),
            // then sets `response._streamId` to the freshly-allocated id.
            // Native Response objects are extensible — the assignment
            // succeeds.
            let stream_id = forward_stream(scope, obj)?;
            classify_stream(scope, status, headers, stream_id)
        }
    }
}

/// Call `globalThis.__zsBeginStreamForward(response)` and return the
/// allocated stream id. Used by both the native and polyfill paths.
fn forward_stream(scope: &mut v8::PinScope, obj: v8::Local<v8::Object>) -> Result<u32, String> {
    let global = scope.get_current_context().global(scope);
    let fn_key = v8::String::new(scope, "__zsBeginStreamForward").unwrap();
    let fn_v = global.get(scope, fn_key.into())
        .ok_or_else(|| "__zsBeginStreamForward missing".to_string())?;
    let forward_fn = v8::Local::<v8::Function>::try_from(fn_v)
        .map_err(|_| "__zsBeginStreamForward not a function".to_string())?;
    let result = forward_fn.call(scope, v8::undefined(scope).into(), &[obj.into()])
        .ok_or_else(|| "__zsBeginStreamForward threw".to_string())?;
    Ok(result.uint32_value(scope).unwrap_or(0))
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
    let s = state.borrow();
    let stream_closed = s.streams.get(&stream_id).map(|ss| ss.closed).unwrap_or(false);

    if stream_closed {
        // Stream is closed — collect buffered chunks as complete body.
        let body_text = s.streams.get(&stream_id)
            .map(|ss| {
                ss.buffer.iter()
                    .map(|b| String::from_utf8_lossy(b).to_string())
                    .collect::<String>()
            })
            .unwrap_or_default();
        drop(s);
        // Clean up the stream state
        state.borrow_mut().streams.remove(&stream_id);
        Ok(ResponseInfo::Complete { status, headers, body: body_text })
    } else {
        drop(s);
        // Stream still open — return as streaming (chunks arrive via timers/async ops)
        Ok(ResponseInfo::Stream { status, headers, stream_id })
    }
}

/// Extract headers from a Response object.
///
/// Fast path: if the Response has a pre-built `_zsHeadersArr` property
/// (populated by `Response.json` and the future Response constructor
/// fast-path), read it directly — one property lookup + array iteration.
/// Skips the Headers instance, the iterable walk, `get_own_property_names`,
/// and the name/value iteration (~8-12 V8 ops per request).
///
/// Slow path: walk `response.headers` via `[Symbol.iterator]()`. This
/// works against both the JS polyfill Headers (which stored a `_map`
/// behind the curtain) and the native Headers IDL surface — both expose
/// `entries()`-style iteration per WHATWG Fetch §2.2.
pub fn extract_response_headers(scope: &mut v8::PinScope, response_obj: v8::Local<v8::Object>) -> Vec<(String, String)> {
    let mut result = Vec::new();

    // --- Fast path: _zsHeadersArr ---
    let fast_key = key(scope, &K_ZS_HEADERS_ARR);
    if let Some(fast_val) = response_obj.get(scope, fast_key.into()) {
        if fast_val.is_array() {
            if let Some(arr_obj) = fast_val.to_object(scope) {
                let len_key = key(scope, &K_LENGTH);
                let len = arr_obj.get(scope, len_key.into())
                    .and_then(|v| v.uint32_value(scope))
                    .unwrap_or(0);
                for i in 0..len {
                    let Some(pair_val) = arr_obj.get_index(scope, i) else { continue };
                    let Some(pair_obj) = pair_val.to_object(scope) else { continue };
                    let Some(name_val) = pair_obj.get_index(scope, 0) else { continue };
                    let Some(val_val) = pair_obj.get_index(scope, 1) else { continue };
                    result.push((
                        name_val.to_rust_string_lossy(scope),
                        val_val.to_rust_string_lossy(scope),
                    ));
                }
                return result;
            }
        }
    }

    // --- Slow path: iterate response.headers via Symbol.iterator ---
    let headers_key = key(scope, &K_HEADERS);
    let Some(headers_val) = response_obj.get(scope, headers_key.into()) else { return result };
    let Some(headers_obj) = headers_val.to_object(scope) else { return result };

    // headers[Symbol.iterator]() — the WebIDL `iterable<>` mixin
    // surface. Polyfill Headers also exposes this via
    // `Headers.prototype[Symbol.iterator] = entries`, so the call site
    // is identical for both implementations.
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
/// Box<ResponseState> (the macro-emitted brand).
///
/// Polyfill Response (deleted in D-23 landing 3): detected via the
/// `__zsResponse = 1` tag on `Response.prototype` (see `embed/fetch.js`).
/// Plain handler returns (`{ status, url }`, primitives, arrays) don't
/// match either.
///
/// Two reads in the worst case (native check + polyfill probe). V8's
/// inline cache turns each lookup into a hidden-class check after warmup,
/// and the polyfill probe goes away with the polyfill in landing 3.
pub fn looks_like_response(scope: &mut v8::PinScope, val: v8::Local<v8::Value>) -> bool {
    // Fast reject: primitives and null can't carry brands.
    if !val.is_object() { return false; }
    let Some(obj) = val.to_object(scope) else { return false; };
    // Native Response brand — internal-field-backed.
    if crate::fetch_response::is_native_response(scope, obj) {
        return true;
    }
    // Polyfill prototype tag (deleted in D-23 landing 3).
    let k = key(scope, &K_ZS_RESPONSE);
    match obj.get(scope, k.into()) {
        Some(v) => v.is_true() || v.uint32_value(scope) == Some(1),
        None => false,
    }
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
