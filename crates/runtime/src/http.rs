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
static K_MAP: v8::OneByteConst = v8::String::create_external_onebyte_const(b"_map");
static K_BODY: v8::OneByteConst = v8::String::create_external_onebyte_const(b"body");
static K_BODY_TEXT: v8::OneByteConst = v8::String::create_external_onebyte_const(b"_bodyText");
static K_IS_STREAM: v8::OneByteConst = v8::String::create_external_onebyte_const(b"_isStreamBody");
static K_WEBSOCKET: v8::OneByteConst = v8::String::create_external_onebyte_const(b"webSocket");
static K_ID: v8::OneByteConst = v8::String::create_external_onebyte_const(b"_id");
static K_LENGTH: v8::OneByteConst = v8::String::create_external_onebyte_const(b"length");
static K_ZS_RESPONSE: v8::OneByteConst = v8::String::create_external_onebyte_const(b"__zsResponse");
static K_MESSAGE: v8::OneByteConst = v8::String::create_external_onebyte_const(b"message");
static K_ZS_HEADERS_ARR: v8::OneByteConst = v8::String::create_external_onebyte_const(b"_zsHeadersArr");

#[inline(always)]
fn key<'s>(scope: &mut v8::PinScope<'s, '_>, k: &'static v8::OneByteConst) -> v8::Local<'s, v8::String> {
    v8::String::new_from_onebyte_const(scope, k).unwrap()
}

// ---------------------------------------------------------------------------
// HTTP helper constants
// ---------------------------------------------------------------------------

/// JS helper compiled once: constructs a `Request` from Rust-supplied params.
///
/// Skips `new Request(url, init)` — that path is ~12 field sets inside a
/// constructor with `input instanceof Request` branches we never take.
/// We build the Request shape directly via `Object.create(Request.prototype)`
/// and inline-set the same fields. Same user-visible semantics (same
/// prototype chain → same `request.text()` / `request.json()` behaviour),
/// measurably less work per hot request.
///
/// Fast paths still preserved:
///   - Skips `JSON.parse` when `headersJson` is empty or `"[]"`.
///   - Skips `_bodyText` copy when body is empty or method is GET/HEAD.
pub const HTTP_CREATE_REQUEST_JS: &str = r#"(function(method, url, headersJson, body) {
    var req = Object.create(Request.prototype);
    req.url = url;
    req.method = method;
    req.redirect = "follow";
    req.signal = null;
    req.cache = "default";
    req.credentials = "same-origin";
    req.mode = "cors";
    req.referrer = "about:client";
    req._bodyUsed = false;
    req._bodyBytes = null;
    req._bodyText = (body && method !== "GET" && method !== "HEAD") ? body : "";
    var hMap = Object.create(null);
    // "[]" is 2 chars; anything longer means at least one real header.
    if (headersJson && headersJson.length > 2) {
        var arr = JSON.parse(headersJson);
        for (var i = 0; i < arr.length; i++) {
            var k = arr[i][0].toLowerCase();
            if (hMap[k]) hMap[k].push(arr[i][1]);
            else hMap[k] = [arr[i][1]];
        }
    }
    var h = Object.create(Headers.prototype);
    h._map = hMap;
    req.headers = h;
    return req;
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

    // Extract headers from response.headers._map
    let headers = extract_response_headers(scope, obj);

    // WebSocket upgrade: status 101 with a `webSocket` property
    if status == 101 {
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

    let is_stream_key = key(scope, &K_IS_STREAM);
    let is_stream = obj.get(scope, is_stream_key.into())
        .map(|v| v.boolean_value(scope))
        .unwrap_or(false);
    if is_stream {
        // Stream ID is on the ReadableStream body: response.body._id
        let body_key = key(scope, &K_BODY);
        let id_key = key(scope, &K_ID);
        let stream_id = obj.get(scope, body_key.into())
            .and_then(|v| v.to_object(scope))
            .and_then(|body_obj| body_obj.get(scope, id_key.into()))
            .and_then(|v| v.uint32_value(scope))
            .unwrap_or(0);

        // Check if the stream is already fully buffered + closed.
        // This handles JS-created ReadableStreams where all chunks were
        // enqueued synchronously in start().
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
    } else {
        let body_key = key(scope, &K_BODY_TEXT);
        let body = obj.get(scope, body_key.into())
            .map(|v| v.to_rust_string_lossy(scope))
            .unwrap_or_default();
        Ok(ResponseInfo::Complete { status, headers, body })
    }
}

/// Extract headers from a Response object.
///
/// Fast path: if the Response has a pre-built `_zsHeadersArr` property
/// (populated by `Response.json` and the future Response constructor
/// fast-path), read it directly — one property lookup + array iteration.
/// Skips the Headers instance, the `_map` walk, `get_own_property_names`,
/// and the name/value iteration (~8-12 V8 ops per request).
///
/// Slow path: fall back to `response.headers._map` walk for user-
/// constructed Responses that don't use the fast path.
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

    // --- Slow path: walk response.headers._map ---
    let headers_key = key(scope, &K_HEADERS);
    let Some(headers_val) = response_obj.get(scope, headers_key.into()) else { return result };
    let Some(headers_obj) = headers_val.to_object(scope) else { return result };
    let map_key = key(scope, &K_MAP);
    let Some(map_val) = headers_obj.get(scope, map_key.into()) else { return result };
    let Some(map_obj) = map_val.to_object(scope) else { return result };

    let Some(names) = map_obj.get_own_property_names(scope, Default::default()) else { return result };
    let len_key = key(scope, &K_LENGTH);
    for i in 0..names.length() {
        let Some(name_val) = names.get_index(scope, i) else { continue };
        let name = name_val.to_rust_string_lossy(scope);
        let Some(arr_val) = map_obj.get(scope, name_val) else { continue };
        let Some(arr_obj) = arr_val.to_object(scope) else { continue };
        let len = arr_obj.get(scope, len_key.into())
            .and_then(|v| v.uint32_value(scope))
            .unwrap_or(0);
        for j in 0..len {
            if let Some(val) = arr_obj.get_index(scope, j) {
                result.push((name.clone(), val.to_rust_string_lossy(scope)));
            }
        }
    }
    result
}

/// Fast test for a V8 value produced by the Response polyfill.
///
/// The polyfill tags `Response.prototype` with `__zsResponse = 1` (see
/// `embed/fetch.js`). Any instance — user-constructed, async-generator
/// wrap, `Response.json/error/redirect` — inherits the tag. Plain handler
/// returns (`{ status, url }`, primitives, arrays) don't.
///
/// One property read per async RPC settlement. The previous probe did two
/// reads (`status` + `headers`) plus two V8 string interns on every call,
/// which showed up in `perf` under fetch-heavy load because fetchExternal
/// resolves to `{ status, url }` — `status` is numeric, so the first read
/// passed and the second always fired. V8's inline cache turns the single
/// lookup into a hidden-class check after warmup.
pub fn looks_like_response(scope: &mut v8::PinScope, val: v8::Local<v8::Value>) -> bool {
    // Fast reject: primitives and null can't inherit a prototype tag.
    if !val.is_object() { return false; }
    let Some(obj) = val.to_object(scope) else { return false; };
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
