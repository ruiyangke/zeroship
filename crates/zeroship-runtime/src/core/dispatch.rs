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
/// `stack`, `code`, `details`, `retryable`.
///
/// The FIELD SET matches the JS-side `errorResponse()` in
/// `init.rs::BOOTSTRAP_JS`. The BEHAVIOUR does not, and this comment used to
/// say it did — "so a procedure throw produces the same body whether the
/// kernel's RPC fast path caught the exception or the slow path's JS handler
/// did". That is false at 5xx and the difference is the whole point of this
/// function: `errorResponse` has no sanitization rail at all, so it forwards
/// `err.message` verbatim at any status. It is reachable only from
/// `fallbackFetch`'s `user.index()` catch, not from the RPC path — see the
/// ticket for the exposure. Do not restore the equivalence claim; if the two
/// are ever made to agree, say which one moved.
///
/// THIS function is what a deployed vite-built app's RPC error actually goes
/// through, and the evidence is the `request_id` format, not a reading of the
/// call graph. A vite-built app DOES install the TS handler as `default.fetch`
/// on both tiers (`sdks/vite-plugin/src/rpc-registry.ts` generates the
/// synthetic entry and calls `createFetchHandler`), so
/// `fetch-handler.ts`'s own 5xx rail is reachable in principle. It did not
/// serve the constraint errors measured by `tests/e2e_dev_vs_deployed_db.sh`:
/// its `newRequestId()` returns a UUID, and those bodies carried
/// `"request_id":"5"` / `"6"` — the u64 counter below. The two emit an
/// identical body shape when there is no code, so the id format is the only
/// thing that distinguishes them.
///
/// The mechanism is the three-tier dispatcher, not a failure of the TS catch.
/// The synthetic entry exports BOTH (`export default { fetch: _zsFetchHandler,
/// rpc: _zsRpc, workflows }`, rpc-registry.ts), and the kernel prefers
/// `default.rpc`. A `/__zeroship/v1/<id>` request taken by that fast path calls
/// the procedure directly, so a throw propagates into native code and lands
/// here. `createFetchHandler`'s own `/__zeroship/v1/` branch does catch — its
/// `rpcAndRespond` opens with a `try` — but it is the fallback for a direct
/// HTTP hit, and the kernel path never enters it. So the TS rail is not
/// bypassed, it is simply not on this route.
///
/// Two independent rails, both client-visible boundaries:
///
/// 1. **5xx body sanitization.** Clients get a fixed response
///    body while raw diagnostics go to the server logs with the request
///    id. `is_public_error_code` codes are exempt from the blanking.
/// 2. **`stack` is never emitted, at ANY status.** Not at 4xx (which
///    skips rail 1 entirely) and not via rail 1's code exemption. See the
///    inline note at the strip for what was measured leaking.
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
        // platform owns the message string. Still logged above.
        // Everything else is blanked.
        //
        // "and there is no stack" USED TO BE ASSERTED HERE. It was not
        // true: this arm skips the blanking, so whatever stack was on the
        // thrown error rode straight out at 500, measured reaching an
        // anonymous caller. The stack strip below is what makes the claim
        // true now; it is not a property of the codes. Do not restore the
        // assertion in place of the enforcement.
        if !extras.code.is_some_and(is_public_error_code) {
            // The code may still ride out alone: see `is_code_only_public_error`
            // for why a constraint violation needs its classification kept and
            // its backend-written wording dropped.
            return build_internal_error_body(
                request_id,
                extras.code.filter(|c| is_code_only_public_error(c)),
            );
        }
    } else if extras.stack.is_some() {
        // 4xx never reached the `tracing::error!` above, so stripping the
        // stack below would otherwise discard it entirely. `debug` keeps it
        // available to an operator without logging a stack on every 404.
        tracing::debug!(
            request_id,
            status,
            error.name = %name,
            error.message = %message,
            error.stack = ?extras.stack,
            "creator app dispatch error (4xx)"
        );
    }

    // THE STACK NEVER GOES ON THE WIRE. Both paths that reach the verbose
    // serializer are client-visible boundaries:
    //
    //   - 4xx, which skips the sanitization rail entirely and is the status
    //     class an ANONYMOUS caller reaches most easily — `requireUser()`'s
    //     401 and `__zsDispatchRpc`'s own 404 `NOT_FOUND` / 400
    //     `INVALID_ARGUMENT` are all platform-minted 4xx, so "the creator
    //     chose to throw it" is not true of the common cases;
    //   - the 5xx whitelist exemption, which was written to keep a
    //     developer-facing `code` on the wire and, being implemented as
    //     "skip the blanking arm", dragged the stack through with it.
    //
    // Measured leaking to an anonymous caller through a real gateway before
    // this strip (tests/e2e_dev_vs_deployed_errors.sh): the app's internal
    // module layout, the dispatcher frame names, and the
    // `__zs_kind_bridge_<hash>` build fingerprint plus byte offsets into the
    // minified server bundle.
    //
    // Only `stack` is removed. `message`, `code`, `details` and `retryable`
    // still ride at 4xx on purpose — creators throw intentional 401/403
    // messages and the SDKs branch on `code`.
    let extras = ErrorExtras { stack: None, ..extras };

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
/// 2. The **creator-facing provisioning refusal** `schema_not_provisioned`
///    (`crates/zeroship-data-orm/src/error.rs`). The app's per-app Postgres role does
///    not exist, so `zeroship migrate` was never run for it. It is on this
///    list because the message is platform-authored, fixed, and interpolates
///    nothing -- the server text and the role name (which embeds the app id)
///    stay in the operator log. Blanking it is what produced the failure this
///    list exists to prevent: a condition with a documented one-command fix
///    arriving as `{"message":"internal error"}`, diagnosable only from a
///    worker log the creator cannot read.
///
/// 3. The **developer-facing DB validation / CAS guardrail** family
///    (`crates/zeroship-data-orm/src/error.rs`). These are `DbError::ValidationFailed`
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
///
/// ## Every code is listed TWICE, and that is not redundancy
///
/// The names above are what `crates/zeroship-data-orm/src/error.rs` throws. They are
/// not what arrives here on the path a creator actually takes.
/// `@zeroship/db` re-stamps every native error through `canonicalErrorCode`
/// (`sdks/db/src/errors.ts:27`) *inside the isolate*, before the throw
/// propagates out to this rail: the generic arm uppercases and
/// underscore-separates, and `CANONICAL_CODE_OVERRIDES` renames two outright
/// (`fk_violation` → `FOREIGN_KEY_VIOLATION`, `version_mismatch` →
/// `OPTIMISTIC_CONCURRENCY`).
///
/// So until 2026-08-10 both allow-lists here tested for a spelling that had
/// already been rewritten one layer down, and every exemption they describe
/// was inert on the SDK path — which is the only path creator code takes.
/// Measured on a live `pnpm dev`: an FK violation reached the caller as a
/// bare `{"message":"internal error","name":"Error","request_id":"24"}` with
/// no code at all. Both spellings are kept because both occur: native for a
/// direct `env.db` call, canonical for anything through the SDK.
///
/// The two lists are hand-mirrored across a language boundary with nothing
/// joining them, so a THIRD override added to `canonicalErrorCode` re-opens
/// this silently. `tests/e2e_dev_vs_deployed_db.sh` is the only instrument
/// that could catch that, and it deliberately asserts the FK by row count
/// rather than by error text.
fn is_public_error_code(code: &str) -> bool {
    matches!(
        code,
        "capability_violation"
            // The app's DB was never provisioned -- plugin-db error.rs.
            // Message is a platform-owned constant; fix is `zeroship migrate`.
            | "schema_not_provisioned"
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
            // The same set as canonicalised by `@zeroship/db`. Note
            // `version_mismatch` becomes `OPTIMISTIC_CONCURRENCY`, not
            // `VERSION_MISMATCH` — the mapping is a table, not a transform.
            | "CAPABILITY_VIOLATION"
            | "SCHEMA_NOT_PROVISIONED"
            | "OPTIMISTIC_CONCURRENCY"
            | "VERSION_FILTER_MUST_BE_TOP_LEVEL"
            | "MULTI_ROW_VERSION_FILTER_UNSUPPORTED"
            | "INVALID_FILTER"
            | "INVALID_COLLECTION"
            | "INVALID_IDENTIFIER"
            | "RESERVED_SYSTEM_FIELD_NAME"
            | "IMMUTABLE_SYSTEM_FIELD"
            | "FILTER_NESTING_TOO_DEEP"
    )
}

/// Codes that survive the 5xx rail WITHOUT their message — the third state
/// between "verbatim" ([`is_public_error_code`]) and "blanked".
///
/// A database constraint violation is developer-facing in its CLASSIFICATION
/// and backend-specific in its WORDING, so the two halves need opposite
/// treatment. plugin-db already classifies all four correctly
/// (`DbError::{UniqueViolation, FkViolation, NotNullViolation, CheckViolation}`,
/// stamped in `to_op_error`); before this existed the rail dropped the code on
/// the floor and a creator inserting a duplicate email got
/// `{"message":"internal error"}` — indistinguishable from the server falling
/// over. Measured end to end by `tests/e2e_dev_vs_deployed_db.sh`.
///
/// The message stays blanked because it is written by the backend, not by us:
/// Postgres says `duplicate key value violates unique constraint
/// "users_email_key"` and SQLite answers with a JSON envelope, so a creator
/// branching on the text would be branching on which tier they are running.
/// The value-bearing `DETAIL:` line is stripped upstream by
/// `scrub_constraint_detail`, so this is a contract-stability rule, not a
/// containment one.
///
/// Deliberately NOT "any string code". The TS rail in
/// `sdks/bootstrap/src/fetch-handler.ts` does preserve any string `.code` at
/// 5xx, which is a live divergence between the two rails; widening this one to
/// match is a contract decision, not a bug fix, and is left open rather than
/// taken here.
/// Both spellings of each code are listed, and the pairing is NOT mechanical:
/// `fk_violation` canonicalises to `FOREIGN_KEY_VIOLATION`, not
/// `FK_VIOLATION`. See [`is_public_error_code`] for why the canonical form is
/// the one that actually arrives.
fn is_code_only_public_error(code: &str) -> bool {
    matches!(
        code,
        // Native spelling — `env.db` called directly, no SDK in the chain.
        "unique_violation" | "fk_violation" | "not_null_violation" | "check_violation"
        // Canonical spelling — anything routed through `@zeroship/db`.
            | "UNIQUE_VIOLATION"
            | "FOREIGN_KEY_VIOLATION"
            | "NOT_NULL_VIOLATION"
            | "CHECK_VIOLATION"
    )
}

fn build_internal_error_body(request_id: u64, code: Option<&str>) -> String {
    let mut out = String::with_capacity(64 + code.map_or(0, |c| c.len() + 10));
    out.push_str(r#"{"message":"internal error","name":"Error""#);
    if let Some(code) = code {
        out.push_str(r#","code":""#);
        escape_json_string(code, &mut out);
        out.push('"');
    }
    out.push_str(r#","request_id":""#);
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
        if let Some(msg_val) = obj.get(scope, msg_key.into())
            && !msg_val.is_undefined()
            && !msg_val.is_null()
        {
            return msg_val.to_rust_string_lossy(scope);
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
        if let Some(name_val) = obj.get(scope, name_key.into())
            && !name_val.is_undefined()
            && !name_val.is_null()
        {
            return name_val.to_rust_string_lossy(scope);
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
        crate::core::invocation::with_captured_context(
            scope,
            &cb.continuation_context,
            |scope| {
                let func = v8::Local::new(scope, &cb.callback);
                let undefined = v8::undefined(scope).into();
                func.call(scope, undefined, &[]);
                crate::core::init::perform_microtask_checkpoint(scope);
            },
        );

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

    /// The spellings in both allow-lists are the NATIVE ones, but nothing a
    /// creator writes reaches this rail carrying them.
    ///
    /// `@zeroship/db` maps every native error through `canonicalErrorCode`
    /// (`sdks/db/src/errors.ts:27`) *inside the isolate*, before the throw
    /// propagates out to this rail. That function uppercases every code and
    /// additionally renames two: `fk_violation` → `FOREIGN_KEY_VIOLATION`
    /// and `version_mismatch` → `OPTIMISTIC_CONCURRENCY`. So the rail was
    /// testing for a spelling that had already been rewritten one layer down.
    ///
    /// MEASURED against a live `pnpm dev` (`examples/db-todos`, port 3061,
    /// 2026-08-10): the same orphan insert reached the sanitizer as the
    /// following two representations before and after verbose serialization:
    ///
    /// ```text
    /// sanitized: {"message":"internal error","name":"Error","request_id":"24"}
    /// verbose:   {"message":"{\"code\":\"fk_violation\",...}",...,
    ///             "code":"FOREIGN_KEY_VIOLATION"}
    /// ```
    ///
    /// The second body is this file's own verbose serializer, so
    /// `extras.code` at the rail is `FOREIGN_KEY_VIOLATION` — `Some`, but
    /// matching neither list, hence blanked to a bare `internal error` with
    /// no code at all. That made the whole code-kept/message-blanked third
    /// state (and the CAS exemption above it) dead on every creator path
    /// that goes through the SDK, which is all of them.
    ///
    /// Both spellings are kept: a caller using `env.db` directly, with no
    /// SDK in the chain, still arrives with the native lowercase code.
    ///
    /// WHAT THIS TEST DOES NOT CATCH: it pins the two spellings that exist
    /// today. If `canonicalErrorCode` grows a third override, nothing here
    /// fails — the lists are hand-mirrored across a language boundary and
    /// only `tests/e2e_dev_vs_deployed_db.sh` exercises the real join.
    #[test]
    fn sdk_canonicalised_codes_survive_the_5xx_sanitization_rail() {
        // (canonical spelling, whether the MESSAGE may also survive)
        let cases = [
            // Renamed by CANONICAL_CODE_OVERRIDES.
            ("FOREIGN_KEY_VIOLATION", false),
            ("OPTIMISTIC_CONCURRENCY", true),
            // Uppercased by the generic arm.
            ("UNIQUE_VIOLATION", false),
            ("NOT_NULL_VIOLATION", false),
            ("CHECK_VIOLATION", false),
            ("VERSION_FILTER_MUST_BE_TOP_LEVEL", true),
            ("INVALID_FILTER", true),
            ("RESERVED_SYSTEM_FIELD_NAME", true),
        ];
        for (code, verbatim) in cases {
            let body =
                build_error_body(500, 42, "the real message", "Error", extras_with_code(code));
            assert!(
                body.contains(&format!(r#""code":"{code}""#)),
                "canonical code {code:?} must survive the 5xx rail, got: {body}"
            );
            if verbatim {
                assert!(
                    !body.contains(r#""message":"internal error""#),
                    "verbatim body expected for {code:?}, got sanitized: {body}"
                );
            } else {
                // Constraint violations keep the classification and drop the
                // backend-written wording — the same split the native
                // spellings get.
                assert!(
                    body.contains(r#""message":"internal error""#),
                    "constraint code {code:?} must keep its message blanked, got: {body}"
                );
            }
        }
    }

    /// THE ANCHOR REGRESSION. A creator who deployed an `env.db` app and
    /// skipped `zeroship migrate` got `{"message":"internal error"}` while
    /// the worker logged `role "app_<uuid>_role" does not exist`. The
    /// sanitiser was right; the classifier was wrong. Now that plugin-db
    /// stamps `schema_not_provisioned`, this rail must let the message
    /// through so the response itself names the command.
    ///
    /// Both spellings, for the reason the sibling test above documents:
    /// `@zeroship/db` re-stamps native codes through `canonicalErrorCode`
    /// inside the isolate, so a creator using the SDK arrives here with the
    /// upper-snake form and a creator calling `env.db` directly with the
    /// native one. Listing only one makes the exemption inert on whichever
    /// path is not listed.
    ///
    /// PAIRED with `genuinely_internal_db_failure_is_still_blanked` below,
    /// which differs in ONE variable: the code. Same status, same message,
    /// same extras. Without that partner this test would only show the rail
    /// emits things, not that it DISCRIMINATES.
    ///
    /// WHAT THIS TEST DOES NOT CATCH: that plugin-db actually produces this
    /// code for a missing role (that is
    /// `crates/zeroship-data-v8/tests/missing_role.rs`), and that the message it pairs with
    /// it names `zeroship migrate` (that is `error.rs`'s own unit test).
    /// This test would pass if the code were stamped on an empty string.
    #[test]
    fn schema_not_provisioned_survives_the_5xx_rail_in_both_spellings() {
        for code in ["schema_not_provisioned", "SCHEMA_NOT_PROVISIONED"] {
            let body = build_error_body(
                500,
                1,
                "this app's database is not provisioned: its per-app Postgres role \
                 does not exist. Run `zeroship migrate` for this app, then retry.",
                "Error",
                extras_with_code(code),
            );
            assert!(
                body.contains(&format!(r#""code":"{code}""#)),
                "code {code:?} must survive the 5xx rail, got: {body}"
            );
            assert!(
                body.contains("zeroship migrate"),
                "the creator must learn the fix from the RESPONSE, not a worker \
                 log they cannot see; got: {body}"
            );
            assert!(
                !body.contains(r#""message":"internal error""#),
                "verbatim body expected for {code:?}, got sanitized: {body}"
            );
        }
    }

    /// THE ONE-VARIABLE CONTROL for the test above. Identical status,
    /// identical message text, identical extras -- only the code differs,
    /// and it is one that is NOT on the allow-list. A genuinely-internal
    /// failure must still be blanked.
    ///
    /// If widening the list for `schema_not_provisioned` had made
    /// everything creator-facing, this fails. That is the whole point of
    /// running it: the sibling test alone cannot tell "the rail passes the
    /// new code" from "the rail stopped blanking anything".
    #[test]
    fn genuinely_internal_db_failure_is_still_blanked() {
        let leaky = "this app's database is not provisioned: its per-app Postgres role \
                     does not exist. Run `zeroship migrate` for this app, then retry.";
        for code in ["internal", "transient", "schema_not_provisioned_typo"] {
            let body = build_error_body(500, 1, leaky, "Error", extras_with_code(code));
            assert_eq!(
                body, r#"{"message":"internal error","name":"Error","request_id":"1"}"#,
                "code {code:?} is not allow-listed and must be blanked, got: {body}"
            );
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

    /// A database CONSTRAINT violation must reach the caller with its
    /// `.code`, and without its message.
    ///
    /// Measured 2026-08-10 by `tests/e2e_dev_vs_deployed_db.sh`: inserting a
    /// todo whose `userId` names no user, and re-inserting a duplicate
    /// `users.email`, both came back as
    /// `{"message":"internal error","name":"Error","request_id":"5"}` — no
    /// code. plugin-db classifies both correctly (`DbError::FkViolation` /
    /// `UniqueViolation`, stamped `fk_violation` / `unique_violation` in
    /// `to_op_error`), so the platform KNOWS which constraint failed and the
    /// creator cannot find out. "Duplicate email" is indistinguishable from
    /// "the server fell over", which is the difference between a form
    /// validation message and a 500 page.
    ///
    /// The message must still be blanked, and that is not belt-and-braces.
    /// Postgres phrases it `duplicate key value violates unique constraint
    /// "users_email_key"` and SQLite phrases it as a JSON envelope; both are
    /// backend-generated, so a creator branching on the text would be
    /// branching on which tier they are running. The value-bearing `DETAIL:`
    /// line is already stripped upstream by `scrub_constraint_detail`
    /// (`crates/zeroship-data-orm/src/error.rs`), so this is about a stable contract,
    /// not about a leak.
    ///
    /// NOT asserted here: that the code is safe to expose in general. This is
    /// a fixed four-member family of platform-owned tokens, not a widening of
    /// the rail — `non_whitelisted_5xx_code_is_still_sanitized` above pins
    /// `transient` to the codeless envelope and must keep passing.
    #[test]
    fn constraint_violation_codes_reach_the_caller_without_their_message() {
        for code in [
            "unique_violation",
            "fk_violation",
            "not_null_violation",
            "check_violation",
        ] {
            let body = build_error_body(
                500,
                5,
                r#"db: duplicate key value violates unique constraint "users_email_key""#,
                "Error",
                extras_with_code(code),
            );
            assert!(
                body.contains(&format!(r#""code":"{code}""#)),
                "code {code:?} must reach the caller, got: {body}"
            );
            assert!(
                body.contains(r#""message":"internal error""#),
                "the backend-specific message must still be blanked for {code:?}, got: {body}"
            );
            assert!(
                !body.contains("users_email_key"),
                "the constraint name must not ride out for {code:?}, got: {body}"
            );
        }
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

    fn extras_with_stack<'a>(stack: &'a str, code: Option<&'a str>) -> ErrorExtras<'a> {
        ErrorExtras {
            stack: Some(stack),
            code,
            ..Default::default()
        }
    }

    /// THE ONE-VARIABLE CONTROL for the two regressions below.
    ///
    /// `build_verbose_error_body` is the serializer both of them reach. Given
    /// the IDENTICAL `ErrorExtras`, it emits the stack. So when
    /// `build_error_body` does not, the difference is the gate — not a
    /// serializer that never had the stack, not an empty body, not a typo in
    /// the assertion string.
    #[test]
    fn the_serializer_itself_can_emit_a_stack() {
        let body = build_verbose_error_body(
            "boom",
            "Error",
            extras_with_stack("Error: boom\n    at handler (app.js:1:1)", None),
        );
        assert!(
            body.contains(r#""stack":"#),
            "the serializer must be able to emit a stack, else the gate tests below are vacuous: {body}"
        );
    }

    /// Regression for the measured production stack leak
    /// (`tests/e2e_dev_vs_deployed_errors.sh`, row `err.status4xx`).
    ///
    /// A 4xx skips the 5xx sanitization rail entirely, so before the fix the
    /// thrown `Error.stack` went verbatim to whoever made the request — and
    /// 4xx is the status class an ANONYMOUS caller can reach most easily
    /// (`requireUser()`'s 401, `__zsDispatchRpc`'s own 404 `NOT_FOUND` and
    /// 400 `INVALID_ARGUMENT`). The measured deployed body carried the
    /// internal module layout, the dispatcher frame names, and the
    /// `__zs_kind_bridge_<hash>` build fingerprint.
    ///
    /// WHAT THIS TEST DOES NOT CATCH: it pins the `stack` key only. It says
    /// nothing about `message`, which is deliberately still forwarded at 4xx
    /// (creators throw intentional 401/403 messages and the SDK branches on
    /// `code`), so a future change that widens what rides along at 4xx would
    /// pass this test.
    #[test]
    fn four_xx_never_ships_a_stack_to_the_client() {
        let body = build_error_body(
            403,
            1,
            "boom",
            "Error",
            extras_with_stack("Error: boom\n    at ei (__user__.js:6:83399)", None),
        );
        assert!(
            !body.contains(r#""stack""#),
            "a 4xx body must not carry a stack, got: {body}"
        );
        // Paired assertion: the body is still the real error, not a blank
        // envelope. Without this, deleting the whole body would pass.
        assert!(body.contains(r#""message":"boom""#), "4xx must keep its message, got: {body}");
    }

    /// Regression for the same leak on its SECOND path — the one the 5xx
    /// rail's own whitelist opened.
    ///
    /// `is_public_error_code` exists so a developer-facing DB/CAS code
    /// (`version_mismatch`, …) survives to the SDK at 500. It is implemented
    /// as "skip the blanking arm", so before the fix everything else in the
    /// envelope survived too — including the stack, at a status the same
    /// module's own comment calls "a public boundary". The exemption's
    /// existing test (`validation_cas_codes_survive_the_5xx_sanitization_rail`)
    /// builds its extras with no stack at all, so it cannot observe this.
    #[test]
    fn whitelisted_5xx_code_exemption_does_not_carry_the_stack() {
        let body = build_error_body(
            500,
            1,
            "boom",
            "Error",
            extras_with_stack(
                "Error: boom\n    at ei (__user__.js:6:83399)",
                Some("version_mismatch"),
            ),
        );
        assert!(
            !body.contains(r#""stack""#),
            "the 5xx whitelist exemption must not drag the stack through, got: {body}"
        );
        // The exemption must still do its job — otherwise this "fix" would be
        // indistinguishable from deleting the whitelist.
        assert!(
            body.contains(r#""code":"version_mismatch""#),
            "the whitelisted code must still survive, got: {body}"
        );
    }
}
