//! Hand-written tests for WebSocket construction — URL parsing,
//! protocol validation, and the WebSocketInit dict.
//!
//! These tests exercise the native `globalThis.WebSocket` class
//! installed by `crate::websocket_native::install_global` when the
//! `runtime_native_websocket` Cargo feature is on.
//!
//! The tests construct sockets but never reach OPEN — the connect
//! task is stubbed in step 2; step 4 wires the handshake. Sends throw
//! InvalidStateError per WHATWG §3.1 step 1 of the send algorithm
//! (CONNECTING → InvalidStateError).
//!
//! Per the design §XII.4: covers URL §IV.2 (ws/wss required, no
//! fragment, etc.), protocol validation (RFC 7230 token rule, no
//! duplicates), and constructor invariants (readyState=CONNECTING,
//! url= serialised, etc.).

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
    let script = match v8::Script::compile(scope, src_v8, None) {
        Some(s) => s,
        None => panic!("compile failed"),
    };
    let result = match script.run(scope) {
        Some(r) => r,
        None => panic!("script run failed"),
    };
    f(result, scope)
}

fn js_string(val: v8::Local<v8::Value>, scope: &mut v8::PinScope) -> String {
    val.to_rust_string_lossy(scope)
}

// ---------------------------------------------------------------------------
// Constructor invariants
// ---------------------------------------------------------------------------

#[test]
fn ws_construct_basic() {
    let s = run_in_v8(
        r#"
        const ws = new WebSocket("wss://example.com/path");
        JSON.stringify({
            url: ws.url,
            readyState: ws.readyState,
            protocol: ws.protocol,
            extensions: ws.extensions,
            bufferedAmount: ws.bufferedAmount,
            binaryType: ws.binaryType,
        });
        "#,
        js_string,
    );
    // Per WHATWG §3.1: readyState=0 (CONNECTING) synchronously,
    // binaryType="blob" by default, bufferedAmount=0.
    assert_eq!(
        s,
        r#"{"url":"wss://example.com/path","readyState":0,"protocol":"","extensions":"","bufferedAmount":0,"binaryType":"blob"}"#
    );
}

#[test]
fn ws_constants_on_constructor_and_proto() {
    let s = run_in_v8(
        r#"
        JSON.stringify({
            ctor: {
                CONNECTING: WebSocket.CONNECTING,
                OPEN: WebSocket.OPEN,
                CLOSING: WebSocket.CLOSING,
                CLOSED: WebSocket.CLOSED,
            },
            proto: {
                CONNECTING: WebSocket.prototype.CONNECTING,
                OPEN: WebSocket.prototype.OPEN,
                CLOSING: WebSocket.prototype.CLOSING,
                CLOSED: WebSocket.prototype.CLOSED,
            },
        });
        "#,
        js_string,
    );
    assert_eq!(
        s,
        r#"{"ctor":{"CONNECTING":0,"OPEN":1,"CLOSING":2,"CLOSED":3},"proto":{"CONNECTING":0,"OPEN":1,"CLOSING":2,"CLOSED":3}}"#
    );
}

#[test]
fn ws_construct_no_url_throws() {
    // Per WebIDL: missing required arg → TypeError.
    let s = run_in_v8(
        r#"
        try {
            new WebSocket();
            "no throw";
        } catch (e) {
            e instanceof TypeError ? "TypeError" : ("Other:" + e.message);
        }
        "#,
        js_string,
    );
    assert_eq!(s, "TypeError");
}

#[test]
fn ws_to_string_tag() {
    let s = run_in_v8(
        r#"
        const ws = new WebSocket("wss://example.com");
        Object.prototype.toString.call(ws);
        "#,
        js_string,
    );
    assert_eq!(s, "[object WebSocket]");
}

#[test]
fn ws_instanceof_event_target() {
    // Per WHATWG: `WebSocket : EventTarget` — the inherit macro chains
    // FunctionTemplate prototypes so instanceof EventTarget is true.
    let s = run_in_v8(
        r#"
        const ws = new WebSocket("wss://example.com");
        JSON.stringify({
            isWebSocket: ws instanceof WebSocket,
            isEventTarget: ws instanceof EventTarget,
        });
        "#,
        js_string,
    );
    assert_eq!(s, r#"{"isWebSocket":true,"isEventTarget":true}"#);
}

// ---------------------------------------------------------------------------
// URL parsing — §IV.2
// ---------------------------------------------------------------------------

#[test]
fn ws_url_http_normalised_to_ws() {
    // WHATWG §3.1 step 4: "If urlRecord's scheme is 'http', then set
    // urlRecord's scheme to 'ws'."
    let s = run_in_v8(
        r#"
        new WebSocket("http://example.com/foo").url;
        "#,
        js_string,
    );
    assert!(s.starts_with("ws://"), "expected ws:// got {s}");
}

#[test]
fn ws_url_https_normalised_to_wss() {
    let s = run_in_v8(
        r#"
        new WebSocket("https://example.com").url;
        "#,
        js_string,
    );
    assert!(s.starts_with("wss://"), "expected wss:// got {s}");
}

#[test]
fn ws_url_invalid_scheme_throws() {
    // WHATWG §3.1 step 6: "If urlRecord's scheme is not 'ws' or 'wss',
    // then throw a SyntaxError DOMException."
    let s = run_in_v8(
        r#"
        try {
            new WebSocket("ftp://example.com");
            "no throw";
        } catch (e) {
            e.message.includes("scheme") ? "scheme-error" : ("other:" + e.message);
        }
        "#,
        js_string,
    );
    assert_eq!(s, "scheme-error");
}

#[test]
fn ws_url_with_fragment_throws() {
    // WHATWG §3.1 step 7: "If urlRecord's fragment is non-null, then
    // throw a SyntaxError DOMException."
    let s = run_in_v8(
        r#"
        try {
            new WebSocket("wss://example.com/path#frag");
            "no throw";
        } catch (e) {
            e.message.includes("fragment") ? "fragment-error" : ("other:" + e.message);
        }
        "#,
        js_string,
    );
    assert_eq!(s, "fragment-error");
}

#[test]
fn ws_url_with_empty_fragment_throws() {
    // The spec treats `wss://x#` as fragment-non-null too — the URL
    // parser sets fragment to "" which is observable.
    let s = run_in_v8(
        r#"
        try {
            new WebSocket("wss://example.com/path#");
            "no throw";
        } catch (e) {
            e.message.includes("fragment") ? "fragment-error" : ("other:" + e.message);
        }
        "#,
        js_string,
    );
    assert_eq!(s, "fragment-error");
}

#[test]
fn ws_url_invalid_string_throws() {
    let s = run_in_v8(
        r#"
        try {
            new WebSocket("not a valid url at all");
            "no throw";
        } catch (e) {
            e instanceof TypeError ? "TypeError" : ("other:" + e.message);
        }
        "#,
        js_string,
    );
    // Either TypeError or some kind of throw — the live spec says
    // SyntaxError DOMException; our shim throws TypeError until
    // DOMException ships natively.
    assert!(s == "TypeError" || s.starts_with("other:"), "got {s}");
}

// ---------------------------------------------------------------------------
// Protocol validation — §IV.2
// ---------------------------------------------------------------------------

#[test]
fn ws_protocol_string_accepted() {
    let s = run_in_v8(
        r#"
        const ws = new WebSocket("wss://example.com", "chat");
        // protocol is empty until the handshake completes; we just
        // verify the constructor accepted the protocol arg.
        ws.protocol;
        "#,
        js_string,
    );
    assert_eq!(s, "");
}

#[test]
fn ws_protocol_array_accepted() {
    let s = run_in_v8(
        r#"
        const ws = new WebSocket("wss://example.com", ["chat", "v2.example"]);
        ws.protocol;
        "#,
        js_string,
    );
    assert_eq!(s, "");
}

#[test]
fn ws_protocol_invalid_token_throws() {
    // RFC 7230 separator chars are forbidden in tokens. Space is one.
    let s = run_in_v8(
        r#"
        try {
            new WebSocket("wss://example.com", "chat with space");
            "no throw";
        } catch (e) {
            e.message.includes("invalid protocol") ? "invalid" : ("other:" + e.message);
        }
        "#,
        js_string,
    );
    assert_eq!(s, "invalid");
}

#[test]
fn ws_protocol_duplicate_throws() {
    // Per WHATWG §3.1 step 9: duplicates fail.
    let s = run_in_v8(
        r#"
        try {
            new WebSocket("wss://example.com", ["chat", "chat"]);
            "no throw";
        } catch (e) {
            e.message.includes("duplicate") ? "dup" : ("other:" + e.message);
        }
        "#,
        js_string,
    );
    assert_eq!(s, "dup");
}

#[test]
fn ws_protocol_duplicate_case_insensitive_throws() {
    // ASCII case-folding for dedup, addresses critic MINOR #34.
    let s = run_in_v8(
        r#"
        try {
            new WebSocket("wss://example.com", ["Chat", "chat"]);
            "no throw";
        } catch (e) {
            e.message.includes("duplicate") ? "dup" : ("other:" + e.message);
        }
        "#,
        js_string,
    );
    assert_eq!(s, "dup");
}

#[test]
fn ws_protocol_empty_string_throws() {
    let s = run_in_v8(
        r#"
        try {
            new WebSocket("wss://example.com", "");
            "no throw";
        } catch (e) {
            e.message.includes("invalid protocol") ? "invalid" : ("other:" + e.message);
        }
        "#,
        js_string,
    );
    assert_eq!(s, "invalid");
}

#[test]
fn ws_protocol_non_ascii_throws() {
    // RFC 7230 token: U+0021..U+007E only.
    let s = run_in_v8(
        r#"
        try {
            new WebSocket("wss://example.com", "ch\u00e4t");
            "no throw";
        } catch (e) {
            e.message.includes("invalid protocol") ? "invalid" : ("other:" + e.message);
        }
        "#,
        js_string,
    );
    assert_eq!(s, "invalid");
}

// ---------------------------------------------------------------------------
// WebSocketInit dictionary
// ---------------------------------------------------------------------------

#[test]
fn ws_init_accepts_origin() {
    // The init dict's `origin` member is opt-in. Its
    // observable effect is on the wire, not on the instance. Just
    // verify the constructor accepts it without throwing.
    let s = run_in_v8(
        r#"
        const ws = new WebSocket("wss://example.com", undefined, {
            origin: "https://my-app.example",
        });
        ws.readyState;
        "#,
        js_string,
    );
    assert_eq!(s, "0"); // CONNECTING
}

#[test]
fn ws_init_accepts_size_caps() {
    let s = run_in_v8(
        r#"
        const ws = new WebSocket("wss://example.com", undefined, {
            maxMessageSize: 1024,
            maxFrameSize: 512,
            pingIntervalMs: 5000,
        });
        ws.readyState;
        "#,
        js_string,
    );
    assert_eq!(s, "0");
}

#[test]
fn ws_init_invalid_object_throws() {
    let s = run_in_v8(
        r#"
        try {
            new WebSocket("wss://example.com", undefined, "not-an-object");
            "no throw";
        } catch (e) {
            e instanceof TypeError ? "TypeError" : ("other:" + e.message);
        }
        "#,
        js_string,
    );
    // String passes V8's `is_object` check as false; we throw TypeError.
    assert_eq!(s, "TypeError");
}

// ---------------------------------------------------------------------------
// readyState transitions (constructor / close / send-when-CONNECTING)
// ---------------------------------------------------------------------------

#[test]
fn ws_send_when_connecting_throws() {
    // Per WHATWG §3.1 send algorithm step 1: throw InvalidStateError
    // when readyState === CONNECTING.
    let s = run_in_v8(
        r#"
        const ws = new WebSocket("wss://example.com");
        try {
            ws.send("hello");
            "no throw";
        } catch (e) {
            e.message.includes("CONNECTING") || e.message.includes("InvalidState")
                ? "InvalidState" : ("other:" + e.message);
        }
        "#,
        js_string,
    );
    assert_eq!(s, "InvalidState");
}

#[test]
fn ws_close_during_connecting_transitions_to_closing() {
    // Per WHATWG §3.1 close algorithm step 3 (CONNECTING branch):
    // run "fail the WebSocket connection". State immediately becomes
    // CLOSING (then CLOSED async — but for the step-2 skeleton we just
    // observe CLOSING since the connect task is stubbed).
    let s = run_in_v8(
        r#"
        const ws = new WebSocket("wss://example.com");
        ws.close();
        ws.readyState; // should be 2 (CLOSING) per the §V.5 algorithm
        "#,
        js_string,
    );
    assert_eq!(s, "2");
}

#[test]
fn ws_close_idempotent_after_closing() {
    let s = run_in_v8(
        r#"
        const ws = new WebSocket("wss://example.com");
        ws.close();
        ws.close(); // second call: no-op
        ws.close(1000, "ok"); // third call: still no-op (already CLOSING)
        ws.readyState;
        "#,
        js_string,
    );
    assert_eq!(s, "2");
}

// ---------------------------------------------------------------------------
// accept() — workerd extension
// ---------------------------------------------------------------------------

#[test]
fn ws_accept_on_client_socket_throws() {
    // Client-side WebSocket throws TypeError on accept().
    let s = run_in_v8(
        r#"
        const ws = new WebSocket("wss://example.com");
        try {
            ws.accept();
            "no throw";
        } catch (e) {
            e instanceof TypeError ? "TypeError" : ("other:" + e.message);
        }
        "#,
        js_string,
    );
    assert_eq!(s, "TypeError");
}
