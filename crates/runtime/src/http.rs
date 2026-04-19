//! HTTP dispatch primitives — pure V8 logic for inspecting Response objects.
//!
//! Extracted from `runtime-tokio` so both `runtime-tokio` and `runtime-compio`
//! can reuse the same V8 property-access and response-inspection code.

use crate::state::SharedState;

// ---------------------------------------------------------------------------
// HTTP helper constants
// ---------------------------------------------------------------------------

/// JS helper compiled once: constructs a `Request` from Rust-supplied params.
pub const HTTP_CREATE_REQUEST_JS: &str = r#"(function(method, url, headersJson, body) {
    var map = Object.create(null);
    if (headersJson) {
        var arr = JSON.parse(headersJson);
        for (var i = 0; i < arr.length; i++) {
            var k = arr[i][0].toLowerCase(), v = arr[i][1];
            if (map[k]) map[k].push(v); else map[k] = [v];
        }
    }
    var init = { method: method, headers: Headers._fromTrusted(map) };
    if (body && method !== "GET" && method !== "HEAD") init.body = body;
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
pub fn inspect_response(scope: &mut v8::PinScope, response_val: v8::Local<v8::Value>) -> Result<ResponseInfo, String> {
    let obj = match response_val.to_object(scope) {
        Some(o) => o,
        None => return Err("Handler did not return a Response object".to_string()),
    };

    let status = get_u32_property(scope, obj, "status") as u16;
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
        let ws_key = v8::String::new(scope, "webSocket").unwrap();
        if let Some(ws_val) = obj.get(scope, ws_key.into()) {
            if !ws_val.is_undefined() && !ws_val.is_null() {
                if let Some(ws_obj) = ws_val.to_object(scope) {
                    let id_key = v8::String::new(scope, "_id").unwrap();
                    let ws_id = ws_obj.get(scope, id_key.into())
                        .and_then(|v| v.uint32_value(scope))
                        .unwrap_or(0);
                    return Ok(ResponseInfo::WebSocket { ws_id, headers });
                }
            }
        }
    }

    let is_stream = get_bool_property(scope, obj, "_isStreamBody");
    if is_stream {
        // Stream ID is on the ReadableStream body: response.body._id
        let body_key = v8::String::new(scope, "body").unwrap();
        let stream_id = obj.get(scope, body_key.into())
            .and_then(|v| v.to_object(scope))
            .map(|body_obj| get_u32_property(scope, body_obj, "_id"))
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
        let body = get_string_property(scope, obj, "_bodyText");
        Ok(ResponseInfo::Complete { status, headers, body })
    }
}

/// Extract headers from a Response object's `headers._map` property.
pub fn extract_response_headers(scope: &mut v8::PinScope, response_obj: v8::Local<v8::Object>) -> Vec<(String, String)> {
    let mut result = Vec::new();
    let headers_key = v8::String::new(scope, "headers").unwrap();
    let Some(headers_val) = response_obj.get(scope, headers_key.into()) else { return result };
    let Some(headers_obj) = headers_val.to_object(scope) else { return result };
    let map_key = v8::String::new(scope, "_map").unwrap();
    let Some(map_val) = headers_obj.get(scope, map_key.into()) else { return result };
    let Some(map_obj) = map_val.to_object(scope) else { return result };

    let Some(names) = map_obj.get_own_property_names(scope, Default::default()) else { return result };
    for i in 0..names.length() {
        let Some(name_val) = names.get_index(scope, i) else { continue };
        let name = name_val.to_rust_string_lossy(scope);
        let Some(arr_val) = map_obj.get(scope, name_val) else { continue };
        let Some(arr_obj) = arr_val.to_object(scope) else { continue };
        let len_key = v8::String::new(scope, "length").unwrap();
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

/// Heuristic test for a V8 value that behaves like a `Response`.
/// Matches the polyfill's shape (numeric `status` + `headers` object)
/// without holding a reference to the polyfill's constructor.
pub fn looks_like_response(scope: &mut v8::PinScope, val: v8::Local<v8::Value>) -> bool {
    let Some(obj) = val.to_object(scope) else { return false; };
    let status_key = v8::String::new(scope, "status").unwrap();
    let Some(s) = obj.get(scope, status_key.into()) else { return false; };
    if !(s.is_int32() || s.is_number()) { return false; }
    let headers_key = v8::String::new(scope, "headers").unwrap();
    let Some(h) = obj.get(scope, headers_key.into()) else { return false; };
    h.is_object()
}

/// Extract the result of a settled promise, branching on RPC vs HTTP.
///
/// RPC path: return the raw handler value as JSON (already the response
/// body for the new wire). If the resolved value looks like a `Response`
/// (e.g. the async-generator wrapper produced one), promote to the Http
/// variant so the streaming machinery takes over.
pub fn extract_settled_result(
    scope: &mut v8::PinScope,
    promise: &v8::Global<v8::Promise>,
    is_http: bool,
) -> SettledResult {
    let local = v8::Local::new(scope, promise);
    match local.state() {
        v8::PromiseState::Fulfilled => {
            let val = local.result(scope);
            if is_http {
                return SettledResult::Http(inspect_response(scope, val));
            }
            // RPC path: promote Response-shaped values to HTTP so the
            // async-generator wrap can stream out exactly as it would from
            // `onRequest`.
            if looks_like_response(scope, val) {
                return SettledResult::Http(inspect_response(scope, val));
            }
            let json = if val.is_undefined() {
                "null".to_string()
            } else {
                v8::json::stringify(scope, val)
                    .map(|s| s.to_rust_string_lossy(scope))
                    .unwrap_or_else(|| "null".to_string())
            };
            SettledResult::Rpc(Ok(json))
        }
        v8::PromiseState::Rejected => {
            // Read `.message` if it's an Error object; else stringify.
            let exc = local.result(scope);
            let msg = if let Some(obj) = exc.to_object(scope) {
                let msg_key = v8::String::new(scope, "message").unwrap();
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
            if is_http {
                SettledResult::Http(Err(msg))
            } else {
                SettledResult::Rpc(Err(msg))
            }
        }
        v8::PromiseState::Pending => {
            let err = "Promise still pending".to_string();
            if is_http {
                SettledResult::Http(Err(err))
            } else {
                SettledResult::Rpc(Err(err))
            }
        }
    }
}
