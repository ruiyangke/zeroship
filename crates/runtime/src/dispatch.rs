//! Free functions for V8 dispatch and resolution.
//!
//! These are free functions (NOT methods) that take `(scope, state, ...)` as
//! separate parameters. This avoids borrow conflicts: when `enter_v8` creates a
//! `HandleScope` borrowing `&mut isolate`, no methods on `&self` can be called.
//! By making them free functions with disjoint `(scope, state)` params, Rust
//! can verify the borrows don't overlap.
//!
//! ## Wire format
//!
//! The wire is URL-path-based (not JSON-RPC):
//!   `POST /_rpc/<methodName>` with body = JSON array of positional args.
//!
//! Response:
//!   - Success (plain value): HTTP 200 + `Content-Type: application/json`
//!     + body = the raw return value JSON.
//!   - Success (async generator): HTTP 200 `text/event-stream` with
//!     `event: yield` / `event: return` / `event: error` frames. DISPATCH_JS
//!     wraps the generator in a `Response(ReadableStream)`, so the streaming
//!     HTTP dispatch path handles delivery exactly like user-constructed
//!     `Response(ReadableStream)` (form B).
//!   - Error: HTTP 500 (or `err.status` if numeric 400-599) + body
//!     `{"message":"...","name":"...","stack":"..."}`.
//!
//! Callers of [`dispatch_request`] classify the result as sync / async /
//! error. For the sync case they receive a `DispatchResult::Sync(ReturnInfo)`
//! whose inner form distinguishes "plain JSON body" from "Response object"
//! (the async-generator wrap path and user-returned `Response`s). The
//! runtime then forwards complete buffered responses or streams them via
//! the existing HTTP infrastructure — no separate SSE framing in Rust.

use crate::state::{DispatchResult, SharedState};

// ---------------------------------------------------------------------------
// Error envelope
// ---------------------------------------------------------------------------

/// Build an error body in the new wire format: `{"message","name","stack"}`.
/// Quotes inside the strings are escaped per JSON rules; no `itoa` / status
/// is embedded — the status is carried separately so streaming HTTP paths
/// can emit the correct status line.
#[inline]
pub fn build_error_body(message: &str, name: &str, stack: Option<&str>) -> String {
    let mut out = String::with_capacity(40 + message.len() + name.len() + stack.map(str::len).unwrap_or(0));
    out.push_str(r#"{"message":""#);
    escape_json_string(message, &mut out);
    out.push_str(r#"","name":""#);
    escape_json_string(name, &mut out);
    if let Some(s) = stack {
        out.push_str(r#"","stack":""#);
        escape_json_string(s, &mut out);
    }
    out.push_str("\"}");
    out
}

/// Escape a string into an in-progress JSON buffer. Quotes are NOT added —
/// the caller positions them. Handles control chars, quote, backslash.
fn escape_json_string(s: &str, out: &mut String) {
    for c in s.chars() {
        match c {
            '"' => out.push_str("\\\""),
            '\\' => out.push_str("\\\\"),
            '\n' => out.push_str("\\n"),
            '\r' => out.push_str("\\r"),
            '\t' => out.push_str("\\t"),
            '\x08' => out.push_str("\\b"),
            '\x0c' => out.push_str("\\f"),
            c if (c as u32) < 0x20 => {
                use std::fmt::Write;
                let _ = write!(out, "\\u{:04x}", c as u32);
            }
            c => out.push(c),
        }
    }
}

// ---------------------------------------------------------------------------
// V8 value serialization
// ---------------------------------------------------------------------------

/// Serialize a V8 value to JSON via `JSON.stringify`. Returns a fallback
/// `"null"` if stringification fails (cycle, non-serializable). Matches
/// the web platform's convention: `undefined` round-trips as `null`.
fn v8_to_json_string(scope: &mut v8::PinScope, value: v8::Local<v8::Value>) -> String {
    if value.is_undefined() {
        return "null".to_string();
    }
    v8::json::stringify(scope, value)
        .map(|s| s.to_rust_string_lossy(scope))
        .unwrap_or_else(|| "null".to_string())
}

/// Extract a human-readable error message from a V8 exception value.
/// Reads `.message` when present (Error instances); falls back to
/// `String(exception)` for other throwables (plain strings, numbers).
pub fn v8_exception_to_message(scope: &mut v8::PinScope, exception: v8::Local<v8::Value>) -> String {
    if let Some(obj) = exception.to_object(scope) {
        let msg_key = v8::String::new(scope, "message").unwrap();
        if let Some(msg_val) = obj.get(scope, msg_key.into()) {
            if !msg_val.is_undefined() && !msg_val.is_null() {
                return msg_val.to_rust_string_lossy(scope);
            }
        }
    }
    exception
        .to_string(scope)
        .map(|s| s.to_rust_string_lossy(scope))
        .unwrap_or_else(|| "unknown error".to_string())
}

/// Read `.name` (Error subclass name) from a V8 exception object. Defaults
/// to `"Error"` for plain throwables.
pub fn v8_exception_to_name(scope: &mut v8::PinScope, exception: v8::Local<v8::Value>) -> String {
    if let Some(obj) = exception.to_object(scope) {
        let name_key = v8::String::new(scope, "name").unwrap();
        if let Some(name_val) = obj.get(scope, name_key.into()) {
            if !name_val.is_undefined() && !name_val.is_null() {
                return name_val.to_rust_string_lossy(scope);
            }
        }
    }
    "Error".to_string()
}

/// Read `.stack` if present. Returns `None` when absent (plain throwables,
/// string errors) so callers can omit the field from the body.
pub fn v8_exception_to_stack(scope: &mut v8::PinScope, exception: v8::Local<v8::Value>) -> Option<String> {
    let obj = exception.to_object(scope)?;
    let key = v8::String::new(scope, "stack").unwrap();
    let val = obj.get(scope, key.into())?;
    if val.is_undefined() || val.is_null() {
        return None;
    }
    Some(val.to_rust_string_lossy(scope))
}

/// Read `.status` as a numeric HTTP status code (400-599). Returns
/// `None` for non-numeric or out-of-range values so callers fall back
/// to HTTP 500.
pub fn v8_exception_to_status(scope: &mut v8::PinScope, exception: v8::Local<v8::Value>) -> Option<u16> {
    let obj = exception.to_object(scope)?;
    let key = v8::String::new(scope, "status").unwrap();
    let val = obj.get(scope, key.into())?;
    let n = val.int32_value(scope)?;
    if (400..=599).contains(&n) { Some(n as u16) } else { None }
}

// ---------------------------------------------------------------------------
// dispatch_request
// ---------------------------------------------------------------------------

/// Call the JS dispatch function with `(methodName, argsJson)` and
/// classify the result.
///
/// Returns:
/// - `DispatchResult::Sync(json)` — synchronous plain-value result, ready
///   to serve as `application/json` body.
/// - `DispatchResult::Async(promise)` — promise is still pending; the
///   caller registers it with the pump. When it settles, the runtime
///   inspects the fulfilled value: if it's a `Response` (user-returned
///   or from the async-generator wrapper) it streams; otherwise plain
///   JSON body.
/// - `DispatchResult::Error(msg)` — hard dispatch error (method lookup
///   failed before entering user code, or an argument too large for V8).
pub fn dispatch_request(
    scope: &mut v8::PinScope,
    _state: &SharedState,
    dispatch_fn: &v8::Global<v8::Function>,
    method: &str,
    args_json: Option<&str>,
) -> DispatchResult {
    let method_arg = match v8::String::new(scope, method) {
        Some(s) => s,
        None => return DispatchResult::Error("Method name too large for V8 string".to_string()),
    };

    let args_arg: v8::Local<v8::Value> = match args_json {
        Some(p) => match v8::String::new(scope, p) {
            Some(s) => s.into(),
            None => return DispatchResult::Error("Args too large for V8 string".to_string()),
        },
        None => v8::null(scope).into(),
    };

    let func = v8::Local::new(scope, dispatch_fn);
    let undefined = v8::undefined(scope).into();

    let (result_val, caught_exception) = {
        v8::tc_scope!(let tc, scope);
        let r = func.call(tc, undefined, &[method_arg.into(), args_arg]);
        if tc.has_caught() {
            let exc = tc.exception();
            let exc_global = exc.map(|e| v8::Global::new(tc, e));
            (None, exc_global)
        } else {
            (r.map(|v| v8::Global::new(tc, v)), None)
        }
    };

    scope.perform_microtask_checkpoint();

    if let Some(exc_global) = caught_exception {
        let exc_local = v8::Local::new(scope, &exc_global);
        return DispatchResult::ErrorValue {
            message: v8_exception_to_message(scope, exc_local),
            name: v8_exception_to_name(scope, exc_local),
            stack: v8_exception_to_stack(scope, exc_local),
            status: v8_exception_to_status(scope, exc_local).unwrap_or(500),
        };
    }

    let Some(result_global) = result_val else {
        return DispatchResult::Error("JS dispatch returned no value".to_string());
    };

    let val = v8::Local::new(scope, &result_global);

    if val.is_promise() {
        let promise = v8::Local::<v8::Promise>::try_from(val).unwrap();
        match promise.state() {
            v8::PromiseState::Fulfilled => {
                let result_val = promise.result(scope);
                // May be a Response (user-returned or async-generator-wrapped)
                // or a plain value. We let the runtime inspect via
                // `extract_promise_result` which classifies both shapes.
                classify_fulfilled(scope, result_val)
            }
            v8::PromiseState::Rejected => {
                let exc = promise.result(scope);
                DispatchResult::ErrorValue {
                    message: v8_exception_to_message(scope, exc),
                    name: v8_exception_to_name(scope, exc),
                    stack: v8_exception_to_stack(scope, exc),
                    status: v8_exception_to_status(scope, exc).unwrap_or(500),
                }
            }
            v8::PromiseState::Pending => {
                let global_promise = v8::Global::new(scope, promise);
                DispatchResult::Async(global_promise)
            }
        }
    } else {
        classify_fulfilled(scope, val)
    }
}

/// Classify a fulfilled value: Response object → forward to HTTP dispatch;
/// plain value → serialize as JSON body.
fn classify_fulfilled(scope: &mut v8::PinScope, val: v8::Local<v8::Value>) -> DispatchResult {
    // Heuristic: a Response has a numeric `status` and a `headers` object.
    // This matches the platform's Response polyfill (and user-constructed
    // `new Response(...)` calls) without importing the polyfill class here.
    if let Some(obj) = val.to_object(scope) {
        let status_key = v8::String::new(scope, "status").unwrap();
        if let Some(s) = obj.get(scope, status_key.into()) {
            if s.is_int32() || s.is_number() {
                let headers_key = v8::String::new(scope, "headers").unwrap();
                if let Some(h) = obj.get(scope, headers_key.into()) {
                    if h.is_object() {
                        // Looks like a Response — use the HTTP inspection path.
                        match crate::http::inspect_response(scope, val) {
                            Ok(info) => return DispatchResult::HttpResponse(info),
                            Err(e) => return DispatchResult::Error(e),
                        }
                    }
                }
            }
        }
    }

    let json = v8_to_json_string(scope, val);
    DispatchResult::Sync(json)
}

// ---------------------------------------------------------------------------
// resolve_op
// ---------------------------------------------------------------------------

/// Resolve a pending promise resolver by op-id.
///
/// Removes the resolver from `state.pending_resolvers`, creates a V8 string
/// from `value`, and resolves the promise. If the value string is too large
/// for V8, the promise is rejected with an error message. Runs a microtask
/// checkpoint after resolution.
pub fn resolve_op(
    scope: &mut v8::PinScope,
    state: &SharedState,
    op_id: u32,
    value: &str,
) {
    let resolver = state.borrow_mut().pending_resolvers.remove(&op_id);
    if let Some(resolver) = resolver {
        let r = v8::Local::new(scope, &resolver);
        match v8::String::new(scope, value) {
            Some(val) => {
                r.resolve(scope, val.into());
            }
            None => {
                let err_msg = v8::String::new(scope, "Op result too large for V8 string")
                    .map(|s| s.into())
                    .unwrap_or_else(|| v8::undefined(scope).into());
                r.reject(scope, err_msg);
            }
        }
        scope.perform_microtask_checkpoint();
    }
}

/// Reject a pending promise resolver by op-id.
///
/// Removes the resolver from `state.pending_resolvers`, creates a V8 `Error`
/// from `error`, and rejects the promise. Runs a microtask checkpoint after
/// rejection.
pub fn reject_op(
    scope: &mut v8::PinScope,
    state: &SharedState,
    op_id: u32,
    error: &str,
) {
    let resolver = state.borrow_mut().pending_resolvers.remove(&op_id);
    if let Some(resolver) = resolver {
        let r = v8::Local::new(scope, &resolver);
        let msg = v8::String::new(scope, error)
            .unwrap_or_else(|| v8::String::new(scope, "unknown error").unwrap());
        let exception = v8::Exception::error(scope, msg);
        r.reject(scope, exception);
        scope.perform_microtask_checkpoint();
    }
}

// ---------------------------------------------------------------------------
// fire_timer_callback
// ---------------------------------------------------------------------------

/// Fire a timer callback by timer-id.
///
/// Removes the callback from `state.timer_callbacks`. If found, calls the
/// function with `undefined` as `this` and no arguments. For `setInterval`
/// timers (`interval.is_some()`), re-inserts the callback for the next fire.
/// Runs a microtask checkpoint after the call.
pub fn fire_timer_callback(
    scope: &mut v8::PinScope,
    state: &SharedState,
    timer_id: u32,
) {
    let cb_opt = state.borrow_mut().timer_callbacks.remove(&timer_id);
    if let Some(cb) = cb_opt {
        let func = v8::Local::new(scope, &cb.callback);
        let undefined = v8::undefined(scope).into();
        func.call(scope, undefined, &[]);
        scope.perform_microtask_checkpoint();

        // setInterval: re-insert so next fire can retrieve it
        if cb.interval.is_some() {
            state.borrow_mut().timer_callbacks.insert(timer_id, cb);
        }
    }
}

// ---------------------------------------------------------------------------
// extract_promise_result
// ---------------------------------------------------------------------------

/// Extract the result of a settled promise as a classified DispatchResult.
///
/// Returns:
/// - `Ok(DispatchResult::Sync(json))` for plain values
/// - `Ok(DispatchResult::HttpResponse(info))` for Response instances
/// - `Err(...)` for rejection or still-pending
pub fn extract_promise_result(
    scope: &mut v8::PinScope,
    promise: &v8::Global<v8::Promise>,
) -> Result<DispatchResult, String> {
    let local = v8::Local::new(scope, promise);
    match local.state() {
        v8::PromiseState::Fulfilled => {
            let result_val = local.result(scope);
            Ok(classify_fulfilled(scope, result_val))
        }
        v8::PromiseState::Rejected => {
            let exc = local.result(scope);
            let msg = v8_exception_to_message(scope, exc);
            Err(msg)
        }
        v8::PromiseState::Pending => Err("Promise still pending".to_string()),
    }
}
