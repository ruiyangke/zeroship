//! Shared V8 dispatch helpers.
//!
//! Free functions (NOT methods) taking `(scope, state, ...)` as separate
//! parameters. The shape avoids borrow conflicts: when `enter_v8!` creates
//! a HandleScope borrowing `&mut isolate`, no methods on `&self` can be
//! called. With disjoint `(scope, state)` params, Rust verifies the borrows
//! don't overlap.
//!
//! Contents:
//!   - error-envelope formatting (`build_error_body`, `escape_json_string`)
//!   - V8 exception extraction (`v8_exception_to_{message,name,stack,status}`)
//!   - native op resolve/reject (`resolve_op`, `reject_op`)
//!   - timer callback firing (`fire_timer_callback`)
//!
//! Wire-level dispatch lives in `runtime.rs::call_fetch_handler` (the
//! kernel's three-tier dispatcher: `default.rpc` → `default.fetchFast` →
//! `default.fetch`). This module is the shared toolkit that path uses.

use crate::state::{DispatchResult, SharedState};

// ---------------------------------------------------------------------------
// Cached property-name keys (perf)
// ---------------------------------------------------------------------------
//
// Each `v8_exception_to_<field>` looks up a constant key
// (`"message"`, `"name"`, `"stack"`, etc) on the exception object. The
// pre-cached keys avoid the per-call `v8::String::new` (UTF-8 validation
// + hash + StringTable internalize), which the bench shows at ~1.8% on
// saturated load. Cached as v8::Globals in an isolate slot so they're
// shared across every call into dispatch.
struct DispatchKeys {
    message: v8::Global<v8::String>,
    name: v8::Global<v8::String>,
    stack: v8::Global<v8::String>,
    status: v8::Global<v8::String>,
    code: v8::Global<v8::String>,
    details: v8::Global<v8::String>,
    retryable: v8::Global<v8::String>,
}

fn dispatch_keys<'s>(scope: &mut v8::PinScope<'s, '_>) -> DispatchKeysLocal<'s> {
    if scope.get_slot::<DispatchKeys>().is_none() {
        let mk = |s: &str| v8::Global::new(scope, v8::String::new(scope, s).unwrap());
        let keys = DispatchKeys {
            message: mk("message"),
            name: mk("name"),
            stack: mk("stack"),
            status: mk("status"),
            code: mk("code"),
            details: mk("details"),
            retryable: mk("retryable"),
        };
        scope.set_slot(keys);
    }
    let slot = scope.get_slot::<DispatchKeys>().unwrap();
    let g = (
        slot.message.clone(),
        slot.name.clone(),
        slot.stack.clone(),
        slot.status.clone(),
        slot.code.clone(),
        slot.details.clone(),
        slot.retryable.clone(),
    );
    DispatchKeysLocal {
        message: v8::Local::new(scope, g.0),
        name: v8::Local::new(scope, g.1),
        stack: v8::Local::new(scope, g.2),
        status: v8::Local::new(scope, g.3),
        code: v8::Local::new(scope, g.4),
        details: v8::Local::new(scope, g.5),
        retryable: v8::Local::new(scope, g.6),
    }
}

struct DispatchKeysLocal<'s> {
    message: v8::Local<'s, v8::String>,
    name: v8::Local<'s, v8::String>,
    stack: v8::Local<'s, v8::String>,
    status: v8::Local<'s, v8::String>,
    code: v8::Local<'s, v8::String>,
    details: v8::Local<'s, v8::String>,
    retryable: v8::Local<'s, v8::String>,
}

// ---------------------------------------------------------------------------
// Error envelope
// ---------------------------------------------------------------------------

/// Optional structured-error extras forwarded from a thrown JS error.
/// `details_json` is pre-serialized so the splicer doesn't have to know
/// the shape (it's any JSON value).
#[derive(Clone, Copy, Default)]
pub struct ErrorExtras<'a> {
    pub stack: Option<&'a str>,
    pub code: Option<&'a str>,
    pub details_json: Option<&'a str>,
    pub retryable: Option<bool>,
}

/// Build the JSON body for an error response.
///
/// Always emitted: `message`, `name`. Optionally appended in this order:
/// `stack`, `code`, `details`, `retryable`. The wire shape matches the
/// JS-side `errorResponse()` in `init.rs::BOOTSTRAP_JS` so a procedure
/// throw produces the same body whether the kernel's RPC fast path
/// caught the exception or the slow path's JS handler did.
///
/// 5xx errors are a public boundary: production clients get a fixed
/// response body while raw diagnostics are emitted to server logs with the
/// request id. `AUTH_INSECURE_DEV=true` (or `1`) preserves the old verbose
/// body for local debugging.
#[inline]
pub fn build_error_body(
    status: u16,
    request_id: u64,
    message: &str,
    name: &str,
    extras: ErrorExtras<'_>,
) -> String {
    if (500..=599).contains(&status) {
        tracing::error!(
            request_id,
            status,
            error.name = %name,
            error.message = %message,
            error.stack = ?extras.stack,
            error.code = ?extras.code,
            error.details = ?extras.details_json,
            error.retryable = ?extras.retryable,
            "creator app dispatch error"
        );
        // Public error codes are platform-generated, secret-free,
        // developer-facing errors (e.g. `capability_violation`). They
        // are safe to surface verbatim even at a 5xx boundary — the
        // platform owns the message string and there is no stack or
        // user-supplied content. Still logged above. Everything else is
        // blanked unless the dev escape hatch is set.
        if !extras.code.is_some_and(is_public_error_code) && !expose_internal_dispatch_errors() {
            return build_internal_error_body(request_id);
        }
    }

    build_verbose_error_body(message, name, extras)
}

/// Error codes the platform generates itself that are safe to surface
/// verbatim to clients even at a 5xx status. These carry no secret
/// content — the message is a fixed platform string with no stack and no
/// user-supplied data — so the 5xx body-sanitization rail exempts them.
///
/// Two families qualify:
///
/// 1. `capability_violation` — the gateway/dispatch capability gate's own
///    refusal (P9).
///
/// 2. The **developer-facing DB validation / CAS guardrail** family
///    (`crates/plugin-db/src/error.rs`). These are `DbError::ValidationFailed`
///    (and `QueryError`-derived) refusals: the plugin rejected the *shape* of
///    a request before touching any row. They are platform-owned static
///    message strings whose only variable content is the caller's own
///    collection / filter shape (never another tenant's data, never a stack).
///    Crucially they carry no 4xx `status` — a native throw materialises a
///    `CodedError` with `.code` + `.hint` but no `.status`, so `statusFromError`
///    defaults them to 500 and the sanitization rail would otherwise strip the
///    very `.code` the SDK branches on (`version_mismatch` →
///    `OptimisticLockError`, etc.). Exempting them keeps the developer-facing
///    code on the wire without leaking anything secret. (Mirrors the TS
///    fetch-handler rail, which already preserves any string `.code` at 5xx.)
fn is_public_error_code(code: &str) -> bool {
    matches!(
        code,
        "capability_violation"
            // Optimistic-concurrency (CAS) guardrails — plugin-db error.rs
            | "version_mismatch"
            | "version_filter_must_be_top_level"
            | "multi_row_version_filter_unsupported"
            // Request-shape validation refusals (QueryError + ValidationFailed)
            | "invalid_filter"
            | "invalid_collection"
            | "invalid_identifier"
            | "reserved_system_field_name"
            | "immutable_system_field"
            | "filter_nesting_too_deep"
    )
}

fn expose_internal_dispatch_errors() -> bool {
    matches!(
        std::env::var("AUTH_INSECURE_DEV").as_deref(),
        Ok("1") | Ok("true") | Ok("TRUE") | Ok("yes") | Ok("YES") | Ok("on") | Ok("ON")
    )
}

fn build_internal_error_body(request_id: u64) -> String {
    let mut out = String::with_capacity(64);
    out.push_str(r#"{"message":"internal error","name":"Error","request_id":""#);
    out.push_str(&request_id.to_string());
    out.push_str(r#""}"#);
    out
}

fn build_verbose_error_body(message: &str, name: &str, extras: ErrorExtras<'_>) -> String {
    let cap = 40
        + message.len()
        + name.len()
        + extras.stack.map(str::len).unwrap_or(0)
        + extras.code.map(str::len).unwrap_or(0)
        + extras.details_json.map(str::len).unwrap_or(0);
    let mut out = String::with_capacity(cap);
    out.push_str(r#"{"message":""#);
    escape_json_string(message, &mut out);
    out.push_str(r#"","name":""#);
    escape_json_string(name, &mut out);
    out.push('"');
    if let Some(s) = extras.stack {
        out.push_str(r#","stack":""#);
        escape_json_string(s, &mut out);
        out.push('"');
    }
    if let Some(c) = extras.code {
        out.push_str(r#","code":""#);
        escape_json_string(c, &mut out);
        out.push('"');
    }
    if let Some(d) = extras.details_json {
        // `details_json` is already valid JSON — splice verbatim.
        out.push_str(r#","details":"#);
        out.push_str(d);
    }
    if let Some(r) = extras.retryable {
        out.push_str(if r { r#","retryable":true"# } else { r#","retryable":false"# });
    }
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

/// Read `.code` (gRPC-style structured error code). Strict — only
/// strings forward; non-string `code` values are dropped to keep the
/// wire contract from drifting.
pub fn v8_exception_to_code(scope: &mut v8::PinScope, exception: v8::Local<v8::Value>) -> Option<String> {
    let obj = exception.to_object(scope)?;
    let key = v8::String::new(scope, "code").unwrap();
    let val = obj.get(scope, key.into())?;
    if !val.is_string() { return None; }
    Some(val.to_rust_string_lossy(scope))
}

/// Read `.details` and JSON-stringify it. Any JSON value is allowed —
/// SDKs (e.g. Zod) attach an issues array here. Returns the serialized
/// JSON so callers can splice it into an error envelope without round-
/// tripping through `serde_json::Value`.
pub fn v8_exception_to_details_json(
    scope: &mut v8::PinScope,
    exception: v8::Local<v8::Value>,
) -> Option<String> {
    let obj = exception.to_object(scope)?;
    let key = v8::String::new(scope, "details").unwrap();
    let val = obj.get(scope, key.into())?;
    if val.is_undefined() { return None; }
    v8::json::stringify(scope, val).map(|s| s.to_rust_string_lossy(scope))
}

/// Read `.retryable` as a boolean hint. Strict — only true booleans
/// forward; truthy non-booleans are dropped.
pub fn v8_exception_to_retryable(
    scope: &mut v8::PinScope,
    exception: v8::Local<v8::Value>,
) -> Option<bool> {
    let obj = exception.to_object(scope)?;
    let key = v8::String::new(scope, "retryable").unwrap();
    let val = obj.get(scope, key.into())?;
    if !val.is_boolean() { return None; }
    Some(val.boolean_value(scope))
}

/// One-shot extractor for a thrown JS error → `DispatchResult::ErrorValue`.
/// Reads message/name/stack/status (always) plus the structured-error
/// extras (code, details, retryable) when the throw shape carries them.
///
/// Uses cached property-name keys (`DispatchKeys`) to avoid 7 per-call
/// `v8::String::new` invocations. The bench showed a flat ~1.8% on those
/// allocations alone in the saturated rejection loop.
pub fn v8_exception_to_error_value(
    scope: &mut v8::PinScope,
    exception: v8::Local<v8::Value>,
) -> DispatchResult {
    let keys = dispatch_keys(scope);
    let obj_opt = exception.to_object(scope);

    let message = match obj_opt {
        Some(obj) => match obj.get(scope, keys.message.into()) {
            Some(v) if !v.is_undefined() && !v.is_null() => v.to_rust_string_lossy(scope),
            _ => exception
                .to_string(scope)
                .map(|s| s.to_rust_string_lossy(scope))
                .unwrap_or_else(|| "unknown error".to_string()),
        },
        None => exception
            .to_string(scope)
            .map(|s| s.to_rust_string_lossy(scope))
            .unwrap_or_else(|| "unknown error".to_string()),
    };

    let name = obj_opt
        .and_then(|obj| obj.get(scope, keys.name.into()))
        .filter(|v| !v.is_undefined() && !v.is_null())
        .map(|v| v.to_rust_string_lossy(scope))
        .unwrap_or_else(|| "Error".to_string());

    let stack = obj_opt
        .and_then(|obj| obj.get(scope, keys.stack.into()))
        .filter(|v| !v.is_undefined() && !v.is_null())
        .map(|v| v.to_rust_string_lossy(scope));

    let status = obj_opt
        .and_then(|obj| obj.get(scope, keys.status.into()))
        .and_then(|v| v.int32_value(scope))
        .filter(|n| (400..=599).contains(n))
        .map(|n| n as u16)
        .unwrap_or(500);

    let code = obj_opt
        .and_then(|obj| obj.get(scope, keys.code.into()))
        .filter(|v| v.is_string())
        .map(|v| v.to_rust_string_lossy(scope));

    let details_json = obj_opt
        .and_then(|obj| obj.get(scope, keys.details.into()))
        .filter(|v| !v.is_undefined())
        .and_then(|v| v8::json::stringify(scope, v))
        .map(|s| s.to_rust_string_lossy(scope));

    let retryable = obj_opt
        .and_then(|obj| obj.get(scope, keys.retryable.into()))
        .filter(|v| v.is_boolean())
        .map(|v| v.boolean_value(scope));

    DispatchResult::ErrorValue {
        message,
        name,
        stack,
        status,
        code,
        details_json,
        retryable,
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
        crate::core::init::perform_microtask_checkpoint(scope);
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
        crate::core::init::perform_microtask_checkpoint(scope);
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
        crate::core::init::perform_microtask_checkpoint(scope);

        // setInterval: re-insert so next fire can retrieve it
        if cb.interval.is_some() {
            state.borrow_mut().timer_callbacks.insert(timer_id, cb);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn extras_with_code(code: &str) -> ErrorExtras<'_> {
        ErrorExtras {
            code: Some(code),
            ..Default::default()
        }
    }

    /// Regression for `sqlite-cas-nested-version-code-dropped`: a
    /// developer-facing DB validation / CAS guardrail throws a
    /// `CodedError` with no `.status`, so `statusFromError` defaults it to
    /// 500. The 5xx sanitization rail must NOT strip the `.code` for this
    /// family — the SDK branches on it (`version_mismatch` →
    /// `OptimisticLockError`, etc.). Pre-fix the rail returned the bare
    /// `{message:"internal error",...}` envelope and dropped the code.
    #[test]
    fn validation_cas_codes_survive_the_5xx_sanitization_rail() {
        for code in [
            "version_filter_must_be_top_level",
            "version_mismatch",
            "multi_row_version_filter_unsupported",
            "invalid_filter",
            "invalid_collection",
            "invalid_identifier",
            "reserved_system_field_name",
            "immutable_system_field",
            "filter_nesting_too_deep",
            // The pre-existing P9 capability gate code must still pass.
            "capability_violation",
        ] {
            let body = build_error_body(500, 42, "the real message", "Error", extras_with_code(code));
            assert!(
                body.contains(&format!(r#""code":"{code}""#)),
                "code {code:?} must survive the 5xx rail, got: {body}"
            );
            assert!(
                !body.contains(r#""message":"internal error""#),
                "verbatim body expected for whitelisted code {code:?}, got sanitized: {body}"
            );
        }
    }

    /// The rail still sanitizes a genuinely-internal 5xx error: a code the
    /// platform does NOT whitelist (e.g. `internal`, a transient DB
    /// failure) must be blanked to the fixed envelope, with no message
    /// leak. This guards against the whitelist being widened into a
    /// blanket "keep every code" rule.
    #[test]
    fn non_whitelisted_5xx_code_is_still_sanitized() {
        let body = build_error_body(
            500,
            7,
            "secret connection string leaked here",
            "Error",
            extras_with_code("transient"),
        );
        assert_eq!(
            body,
            r#"{"message":"internal error","name":"Error","request_id":"7"}"#,
            "non-whitelisted 5xx code must be sanitized"
        );
        assert!(!body.contains("secret connection string"));
    }

    /// 4xx errors are a developer-facing boundary — the rail leaves them
    /// verbatim regardless of code (the sanitization only applies to
    /// 500-599).
    #[test]
    fn four_xx_errors_are_not_sanitized() {
        let body = build_error_body(400, 1, "bad request", "Error", extras_with_code("whatever"));
        assert!(body.contains("bad request"));
        assert!(body.contains(r#""code":"whatever""#));
    }
}
