//! Free functions for V8 dispatch and resolution.
//!
//! These are free functions (NOT methods) that take `(scope, state, ...)` as
//! separate parameters. This avoids borrow conflicts: when `enter_v8` creates a
//! `HandleScope` borrowing `&mut isolate`, no methods on `&self` can be called.
//! By making them free functions with disjoint `(scope, state)` params, Rust
//! can verify the borrows don't overlap.
//!
//! ## JSON-RPC envelope — moved out of V8
//!
//! An earlier revision wrapped every dispatch in a JS IIFE that did
//! `JSON.parse(requestBody)` then `JSON.stringify(envelope)`. For the
//! ping hot path (tiny payload, no work), those two JSON calls cost ~15%
//! of total CPU per request. They now happen on the Rust side:
//!
//! - Parse the incoming JSON-RPC body with `parse_request` (serde_json
//!   uses `RawValue` to avoid re-allocating `params` and `id`).
//! - DISPATCH_JS takes `(method, paramsJson)` and returns the raw handler
//!   value (or throws).
//! - This module calls `v8::json::stringify` on the returned value to
//!   serialize the result, then builds the envelope via `format!`.
//!
//! The id is preserved verbatim (`RawValue`) so it round-trips unchanged
//! — a number stays a number, a string stays a string, and `null` / absent
//! are both serialized as `null` per JSON-RPC 2.0 §5.

use crate::state::{DispatchResult, SharedState};

// ---------------------------------------------------------------------------
// JSON-RPC request parsing
// ---------------------------------------------------------------------------

/// Parsed view of an incoming JSON-RPC request. Holds borrowed slices from
/// the original body — no allocations for `params` or `id`, which are kept
/// as `RawValue` so they round-trip without re-serialization.
#[derive(serde::Deserialize)]
pub struct JsonRpcRequestBorrow<'a> {
    pub method: &'a str,
    #[serde(default, borrow)]
    pub params: Option<&'a serde_json::value::RawValue>,
    #[serde(default, borrow)]
    pub id: Option<&'a serde_json::value::RawValue>,
}

/// Parse an incoming JSON-RPC request body.
///
/// Returns `Err(envelope)` when the body is malformed — the envelope is
/// already the full wire error, so callers can just pass it through.
pub fn parse_request(body: &str) -> Result<JsonRpcRequestBorrow<'_>, String> {
    serde_json::from_str::<JsonRpcRequestBorrow>(body)
        .map_err(|_| build_error_envelope(-32700, "Parse error", "null"))
}

// ---------------------------------------------------------------------------
// Envelope builders
// ---------------------------------------------------------------------------

/// Build a JSON-RPC 2.0 success envelope.
///
/// `result_json` is an already-serialized JSON value (e.g. `"pong"`, `7`,
/// `{...}`). `id_json` is the raw id slice from the request body — it's
/// inserted verbatim so types are preserved (number → number, etc.).
#[inline]
pub fn build_success_envelope(result_json: &str, id_json: &str) -> String {
    // Pre-size: prefix(~25) + result + mid(~7) + id + suffix(~1).
    let mut out = String::with_capacity(40 + result_json.len() + id_json.len());
    out.push_str(r#"{"jsonrpc":"2.0","result":"#);
    out.push_str(result_json);
    out.push_str(r#","id":"#);
    out.push_str(id_json);
    out.push('}');
    out
}

/// Build a JSON-RPC 2.0 error envelope.
#[inline]
pub fn build_error_envelope(code: i32, message: &str, id_json: &str) -> String {
    let mut out = String::with_capacity(80 + message.len() + id_json.len());
    out.push_str(r#"{"jsonrpc":"2.0","error":{"code":"#);
    let mut buf = itoa::Buffer::new();
    out.push_str(buf.format(code));
    out.push_str(r#","message":""#);
    // Escape the message per JSON string rules.
    escape_json_string(message, &mut out);
    out.push_str(r#""},"id":"#);
    out.push_str(id_json);
    out.push('}');
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
/// `"null"` if stringification fails (e.g. the value contains a cycle or
/// a non-serializable like a function — those round-trip as `undefined`
/// in JS, which JSON.stringify represents by omitting the key; for a
/// top-level value we produce `null` to keep the envelope well-formed).
fn v8_to_json_string(scope: &mut v8::PinScope, value: v8::Local<v8::Value>) -> String {
    if value.is_undefined() {
        // JSON.stringify(undefined) returns `undefined` (not a string),
        // which violates our envelope contract. Treat as null — the old
        // JS wrapper's `{result: v}` with v=undefined dropped the field
        // entirely, so serving null here is at least as informative.
        return "null".to_string();
    }
    v8::json::stringify(scope, value)
        .map(|s| s.to_rust_string_lossy(scope))
        .unwrap_or_else(|| "null".to_string())
}

/// Extract a human-readable error message from a V8 exception value.
/// Reads `.message` when present (Error instances); falls back to
/// `String(exception)` for other throwables (plain strings, numbers).
fn v8_exception_to_message(scope: &mut v8::PinScope, exception: v8::Local<v8::Value>) -> String {
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

/// Read an optional `code` property from a V8 exception object.
/// Defaults to -32000 (application error) per our JSON-RPC convention.
fn v8_exception_to_code(scope: &mut v8::PinScope, exception: v8::Local<v8::Value>) -> i32 {
    if let Some(obj) = exception.to_object(scope) {
        let code_key = v8::String::new(scope, "code").unwrap();
        if let Some(code_val) = obj.get(scope, code_key.into()) {
            if let Some(n) = code_val.int32_value(scope) {
                return n;
            }
        }
    }
    -32000
}

// ---------------------------------------------------------------------------
// dispatch_request
// ---------------------------------------------------------------------------

/// Call the JS dispatch function with a parsed request and classify the result.
///
/// Returns:
/// - `DispatchResult::Sync(envelope)` — synchronous result or already-settled
///   promise; the envelope is already the full wire JSON.
/// - `DispatchResult::Async(promise)` — promise is still pending; the caller
///   must remember `id_json` to build the envelope when it settles.
/// - `DispatchResult::Error(msg)` — hard error (body too large, etc.) —
///   callers wrap this in their own error envelope.
pub fn dispatch_request(
    scope: &mut v8::PinScope,
    _state: &SharedState,
    dispatch_fn: &v8::Global<v8::Function>,
    method: &str,
    params_json: Option<&str>,
    id_json: &str,
) -> DispatchResult {
    // Build JS args. Passing None for params avoids a JSON.parse call in JS
    // on the ping hot path (no params → empty array handled in JS).
    let method_arg = match v8::String::new(scope, method) {
        Some(s) => s,
        None => return DispatchResult::Error("Method name too large for V8 string".to_string()),
    };

    let params_arg: v8::Local<v8::Value> = match params_json {
        Some(p) => match v8::String::new(scope, p) {
            Some(s) => s.into(),
            None => return DispatchResult::Error("Params too large for V8 string".to_string()),
        },
        None => v8::null(scope).into(),
    };

    let func = v8::Local::new(scope, dispatch_fn);
    let undefined = v8::undefined(scope).into();

    // Call inside a TryCatch so we can distinguish throws from normal returns.
    // The old JS IIFE caught exceptions internally and returned an envelope
    // string. Now the wrapper rethrows — Rust catches and builds the envelope.
    let (result_val, caught_exception) = {
        v8::tc_scope!(let tc, scope);
        let r = func.call(tc, undefined, &[method_arg.into(), params_arg]);
        if tc.has_caught() {
            let exc = tc.exception();
            // Global-ize so we can use it outside the try-catch scope.
            let exc_global = exc.map(|e| v8::Global::new(tc, e));
            (None, exc_global)
        } else {
            (r.map(|v| v8::Global::new(tc, v)), None)
        }
    };

    scope.perform_microtask_checkpoint();

    if let Some(exc_global) = caught_exception {
        let exc_local = v8::Local::new(scope, &exc_global);
        let code = v8_exception_to_code(scope, exc_local);
        let msg = v8_exception_to_message(scope, exc_local);
        return DispatchResult::Sync(build_error_envelope(code, &msg, id_json));
    }

    let Some(result_global) = result_val else {
        // call() returned None but TryCatch caught nothing — defensive.
        return DispatchResult::Sync(build_error_envelope(
            -32000,
            "JS dispatch returned no value",
            id_json,
        ));
    };

    let val = v8::Local::new(scope, &result_global);

    if val.is_promise() {
        let promise = v8::Local::<v8::Promise>::try_from(val).unwrap();
        match promise.state() {
            v8::PromiseState::Fulfilled => {
                let result_val = promise.result(scope);
                let result_json = v8_to_json_string(scope, result_val);
                DispatchResult::Sync(build_success_envelope(&result_json, id_json))
            }
            v8::PromiseState::Rejected => {
                let exc = promise.result(scope);
                let code = v8_exception_to_code(scope, exc);
                let msg = v8_exception_to_message(scope, exc);
                DispatchResult::Sync(build_error_envelope(code, &msg, id_json))
            }
            v8::PromiseState::Pending => {
                let global_promise = v8::Global::new(scope, promise);
                DispatchResult::Async(global_promise)
            }
        }
    } else {
        let result_json = v8_to_json_string(scope, val);
        DispatchResult::Sync(build_success_envelope(&result_json, id_json))
    }
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

/// Extract the result of a settled promise as a JSON-RPC envelope.
///
/// Returns `Ok(envelope)` if fulfilled, `Err(msg)` if rejected or still
/// pending. The envelope is the full wire JSON — ready to send.
pub fn extract_promise_result(
    scope: &mut v8::PinScope,
    promise: &v8::Global<v8::Promise>,
    id_json: &str,
) -> Result<String, String> {
    let local = v8::Local::new(scope, promise);
    match local.state() {
        v8::PromiseState::Fulfilled => {
            let result_val = local.result(scope);
            let result_json = v8_to_json_string(scope, result_val);
            Ok(build_success_envelope(&result_json, id_json))
        }
        v8::PromiseState::Rejected => {
            let exc = local.result(scope);
            let msg = v8_exception_to_message(scope, exc);
            Err(msg)
        }
        v8::PromiseState::Pending => Err("Promise still pending".to_string()),
    }
}
