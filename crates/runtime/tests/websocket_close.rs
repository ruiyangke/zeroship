//! Hand-written tests for `WebSocket.close()` validation.
//!
//! Per WHATWG §3.1 close algorithm + the v2 design's [Clamp]
//! correctness:
//!   - `code` is `[Clamp] unsigned short`: NaN→0, sign-aware clamp
//!     to [0,65535], banker's rounding for .5 ties.
//!   - Close-code validation: 1000 OR 3000-4999; everything else
//!     throws InvalidAccessError DOMException.
//!   - Reason length: ≤ 123 UTF-8 bytes; longer throws SyntaxError.

#![cfg(feature = "runtime_native_websocket")]
#![allow(unsafe_code)]

use zeroship_runtime::dom;
use zeroship_runtime::init_v8;
use zeroship_runtime::websocket_native;

fn run_in_v8<F, R>(src: &str, f: F) -> R
where
    F: FnOnce(v8::Local<v8::Value>, &mut v8::PinScope) -> R,
{
    init_v8();
    let mut isolate = v8::Isolate::new(v8::CreateParams::default());
    v8::scope!(let handle_scope, &mut isolate);
    let context = v8::Context::new(handle_scope, Default::default());
    let scope = &mut v8::ContextScope::new(handle_scope, context);

    let global = scope.get_current_context().global(scope);
    dom::install_globals(scope, global);
    websocket_native::install_global(scope, global);

    let src_v8 = v8::String::new(scope, src).unwrap();
    let script = v8::Script::compile(scope, src_v8, None).unwrap();
    let result = script.run(scope).unwrap();
    f(result, scope)
}

fn js_string(val: v8::Local<v8::Value>, scope: &mut v8::PinScope) -> String {
    val.to_rust_string_lossy(scope)
}

// ---------------------------------------------------------------------------
// Allowed code ranges
// ---------------------------------------------------------------------------

#[test]
fn close_with_1000_is_allowed() {
    let s = run_in_v8(
        r#"
        const ws = new WebSocket("wss://example.com");
        ws.close(1000); // 1000 is the canonical "normal closure" code
        ws.readyState;
        "#,
        js_string,
    );
    assert_eq!(s, "2"); // CLOSING
}

#[test]
fn close_with_3000_is_allowed() {
    let s = run_in_v8(
        r#"
        const ws = new WebSocket("wss://example.com");
        ws.close(3000);
        ws.readyState;
        "#,
        js_string,
    );
    assert_eq!(s, "2");
}

#[test]
fn close_with_4999_is_allowed() {
    let s = run_in_v8(
        r#"
        const ws = new WebSocket("wss://example.com");
        ws.close(4999);
        ws.readyState;
        "#,
        js_string,
    );
    assert_eq!(s, "2");
}

#[test]
fn close_with_no_args_is_allowed() {
    // Per WHATWG §3.1: code is optional. None on the wire = empty
    // payload Close frame per RFC 6455 §5.5.1.
    let s = run_in_v8(
        r#"
        const ws = new WebSocket("wss://example.com");
        ws.close();
        ws.readyState;
        "#,
        js_string,
    );
    assert_eq!(s, "2");
}

// ---------------------------------------------------------------------------
// Disallowed code ranges (throws InvalidAccessError)
// ---------------------------------------------------------------------------

#[test]
fn close_with_999_throws() {
    let s = run_in_v8(
        r#"
        const ws = new WebSocket("wss://example.com");
        try {
            ws.close(999);
            "no throw";
        } catch (e) {
            e.message.includes("InvalidAccess") ? "InvalidAccess" : ("other:" + e.message);
        }
        "#,
        js_string,
    );
    assert_eq!(s, "InvalidAccess");
}

#[test]
fn close_with_1001_through_2999_all_throw() {
    // Per WHATWG §3.1: only 1000 + 3000-4999 are allowed for the close()
    // method. Everything else (including the protocol-level codes
    // 1001-2999) is forbidden.
    let s = run_in_v8(
        r#"
        const codes = [1001, 1006, 1011, 2000, 2999];
        const results = codes.map(c => {
            const ws = new WebSocket("wss://example.com");
            try { ws.close(c); return "no throw"; }
            catch (e) { return e.message.includes("InvalidAccess") ? "InvalidAccess" : "other"; }
        });
        JSON.stringify(results);
        "#,
        js_string,
    );
    assert_eq!(
        s,
        r#"["InvalidAccess","InvalidAccess","InvalidAccess","InvalidAccess","InvalidAccess"]"#
    );
}

#[test]
fn close_with_5000_throws() {
    let s = run_in_v8(
        r#"
        const ws = new WebSocket("wss://example.com");
        try {
            ws.close(5000);
            "no throw";
        } catch (e) {
            e.message.includes("InvalidAccess") ? "InvalidAccess" : ("other:" + e.message);
        }
        "#,
        js_string,
    );
    assert_eq!(s, "InvalidAccess");
}

#[test]
fn close_with_65535_throws() {
    // 65535 is the IDL max for unsigned short — but it's not in the
    // allowed list. After [Clamp] passes through, validation rejects.
    let s = run_in_v8(
        r#"
        const ws = new WebSocket("wss://example.com");
        try {
            ws.close(65535);
            "no throw";
        } catch (e) {
            e.message.includes("InvalidAccess") ? "InvalidAccess" : ("other:" + e.message);
        }
        "#,
        js_string,
    );
    assert_eq!(s, "InvalidAccess");
}

// ---------------------------------------------------------------------------
// [Clamp] correctness.
// Earlier versions used `uint32().min(65535)`, which does not match
// WebIDL's `ConvertToInt[Clamp]`.
// ---------------------------------------------------------------------------

#[test]
fn close_clamp_negative_to_zero_then_validate_rejects() {
    // -1 → [Clamp] → 0 → validate rejects (0 not in allowed list).
    // The v1 design used uint32_value(scope).unwrap_or(0) which reads
    // V8's modulo-2^32 conversion: -1 → 4294967295 → min(65535) →
    // 65535. The correct [Clamp] for -1 is 0, which is also invalid
    // — the OBSERVABLE is the same throw, but the route is different.
    let s = run_in_v8(
        r#"
        const ws = new WebSocket("wss://example.com");
        try {
            ws.close(-1);
            "no throw";
        } catch (e) {
            e.message.includes("InvalidAccess") ? "InvalidAccess" : ("other:" + e.message);
        }
        "#,
        js_string,
    );
    assert_eq!(s, "InvalidAccess");
}

#[test]
fn close_clamp_nan_to_zero_then_validate_rejects() {
    // NaN → [Clamp] → 0 → reject.
    let s = run_in_v8(
        r#"
        const ws = new WebSocket("wss://example.com");
        try {
            ws.close(NaN);
            "no throw";
        } catch (e) {
            e.message.includes("InvalidAccess") ? "InvalidAccess" : ("other:" + e.message);
        }
        "#,
        js_string,
    );
    assert_eq!(s, "InvalidAccess");
}

#[test]
fn close_clamp_infinity_to_max() {
    // +Infinity → [Clamp] → 65535 → reject (65535 invalid).
    let s = run_in_v8(
        r#"
        const ws = new WebSocket("wss://example.com");
        try {
            ws.close(Infinity);
            "no throw";
        } catch (e) {
            e.message.includes("InvalidAccess") ? "InvalidAccess" : ("other:" + e.message);
        }
        "#,
        js_string,
    );
    assert_eq!(s, "InvalidAccess");
}

#[test]
fn close_clamp_floating_in_range_truncates() {
    // 1000.4 → [Clamp] → round to nearest → 1000 (allowed).
    let s = run_in_v8(
        r#"
        const ws = new WebSocket("wss://example.com");
        ws.close(1000.4); // → 1000 (round-to-nearest, not a tie)
        ws.readyState;
        "#,
        js_string,
    );
    assert_eq!(s, "2");
}

#[test]
fn close_clamp_tie_rounds_to_even() {
    // 999.5 → [Clamp] tie → round to even → 1000 (1000 is even and is
    // allowed). The v1 design's `n.uint32_value().min(65535)` would
    // truncate to 999 (rejected) instead.
    let s = run_in_v8(
        r#"
        const ws = new WebSocket("wss://example.com");
        ws.close(999.5); // → 1000 (banker's rounding)
        ws.readyState;
        "#,
        js_string,
    );
    assert_eq!(s, "2");
}

// ---------------------------------------------------------------------------
// Reason validation
// ---------------------------------------------------------------------------

#[test]
fn close_reason_short_accepted() {
    let s = run_in_v8(
        r#"
        const ws = new WebSocket("wss://example.com");
        ws.close(1000, "Goodbye");
        ws.readyState;
        "#,
        js_string,
    );
    assert_eq!(s, "2");
}

#[test]
fn close_reason_exactly_123_bytes_ok() {
    let s = run_in_v8(
        r#"
        // 123 ASCII chars = 123 UTF-8 bytes — at the boundary.
        const reason = "x".repeat(123);
        const ws = new WebSocket("wss://example.com");
        ws.close(1000, reason);
        ws.readyState;
        "#,
        js_string,
    );
    assert_eq!(s, "2");
}

#[test]
fn close_reason_over_123_bytes_throws() {
    // 124 ASCII chars > 123 bytes → SyntaxError.
    let s = run_in_v8(
        r#"
        const reason = "x".repeat(124);
        const ws = new WebSocket("wss://example.com");
        try {
            ws.close(1000, reason);
            "no throw";
        } catch (e) {
            e.message.includes("Syntax") ? "Syntax" : ("other:" + e.message);
        }
        "#,
        js_string,
    );
    assert_eq!(s, "Syntax");
}

#[test]
fn close_reason_byte_count_not_codepoint_count() {
    // Per spec: "UTF-8 encode" then byte-length check. 41 emoji × 4
    // UTF-8 bytes each = 164 bytes > 123 — must throw.
    let s = run_in_v8(
        r#"
        // "🚀" is U+1F680, encoded as 4 UTF-8 bytes.
        const reason = "🚀".repeat(41);
        const ws = new WebSocket("wss://example.com");
        try {
            ws.close(1000, reason);
            "no throw";
        } catch (e) {
            e.message.includes("Syntax") ? "Syntax" : ("other:" + e.message);
        }
        "#,
        js_string,
    );
    assert_eq!(s, "Syntax");
}

#[test]
fn close_reason_validation_runs_before_state_transition() {
    // Per WHATWG: validation (steps 1-2) precedes the close-the-connection
    // step (step 3). A reason-too-long throw must NOT have transitioned
    // the readyState.
    let s = run_in_v8(
        r#"
        const ws = new WebSocket("wss://example.com");
        try {
            ws.close(1000, "x".repeat(200));
        } catch (_) {}
        ws.readyState;
        "#,
        js_string,
    );
    assert_eq!(s, "0"); // still CONNECTING
}

#[test]
fn close_invalid_code_validation_before_state_transition() {
    // Same invariant for the code-invalid path.
    let s = run_in_v8(
        r#"
        const ws = new WebSocket("wss://example.com");
        try {
            ws.close(1001);
        } catch (_) {}
        ws.readyState;
        "#,
        js_string,
    );
    assert_eq!(s, "0");
}
