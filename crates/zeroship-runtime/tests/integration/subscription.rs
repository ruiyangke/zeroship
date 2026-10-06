// Subscription dispatch over WebSocket.
//
// These tests reach into `state.native_websockets[ws_id].events` (the
// per-WS event queue). The semantic shape is the same - outgoing frames
// the server-side WS sends arrive in the CLIENT-side native_websockets'
// `events` queue (via `pair::deliver_to_peer`), and we drive the
// server-side `_onMessage` / `_onClose` by pushing `WsEvent::*` onto
// its events queue and letting the pump dispatch.
//
// The native runtime captures the procedure dictionary when it accepts an
// upgrade, then resolves and invokes the string wire id after the hello frame.
// Frame protocol:
//
//   client → server (first):  {"t":"hello","input":<json>}
//   server → client:           {"t":"data","value":<json>}   each yield
//                              {"t":"end"}                   normal completion
//                              {"t":"error","error":<env>}   on throw
//                              ping/pong                     keepalive
//
// These tests drive the full path: synthesize a WS-upgrade GET to
// `/__zeroship/v1/<id>`, intercept the server-side WebSocket of the pair,
// inject a `hello` event on the server side, drain outgoing events
// from the client side as the generator progresses.

use std::time::Duration;

use futures::StreamExt;
use zeroship_runtime::channel::CancelFlag;
use zeroship_runtime::runtime::Runtime;
use zeroship_runtime::websocket_native::network::WsEvent;
use zeroship_runtime::{EnvSnapshot, FetchOutcome, ModuleEntry, RequestCtx, init_v8};

// ── Test scaffolding ────────────────────────────────────────────────────

/// Send a WS-upgrade GET to `/__zeroship/v1/<id>`. Returns the ws_id of the
/// CLIENT side of the pair (the one returned to the kernel) — the
/// server side is `client_ws_id + 1` (the next ID). Also enables the
/// per-WS event log on both halves so the test can observe events
/// without racing the pump's dispatch drain.
fn upgrade(runtime: &Runtime) -> u32 {
    let env = EnvSnapshot::empty();
    let ctx = RequestCtx::new(CancelFlag::new());
    let outcome = runtime.call_fetch_handler(
        "GET",
        "http://localhost/__zeroship/v1/sub",
        &[
            ("connection".into(), "Upgrade".into()),
            ("upgrade".into(), "websocket".into()),
        ],
        "",
        &env,
        ctx,
    );
    let client_id = match outcome {
        FetchOutcome::WebSocketUpgrade { ws_id, .. } => ws_id,
        other => match other {
            FetchOutcome::Response { status, body, .. } => {
                let body = String::from_utf8_lossy(&body).into_owned();
                panic!("expected WS upgrade, got Response status={status} body={body}")
            }
            _ => panic!("expected WS upgrade, got non-Response outcome"),
        },
    };
    enable_event_logs(runtime, client_id, server_id(client_id));
    client_id
}

/// Server-side ws_id for a pair where `client_id` is the first half.
fn server_id(client_id: u32) -> u32 {
    // WebSocketPair allocates two consecutive ids, client first.
    client_id + 1
}

/// Inject an event on the SERVER side of the pair (simulating an
/// incoming frame from the client) and wake the pump. Subscription-owned
/// sockets are intercepted by the native RPC transport before DOM dispatch.
fn inject_server_message(runtime: &Runtime, server_ws_id: u32, data: &str) {
    inject_server_event(runtime, server_ws_id, WsEvent::MessageText(data.into()));
}

fn inject_server_close(runtime: &Runtime, server_ws_id: u32, code: u16, reason: &str) {
    inject_server_event(
        runtime,
        server_ws_id,
        WsEvent::Close {
            code,
            reason: reason.into(),
            was_clean: true,
        },
    );
}

fn inject_server_event(runtime: &Runtime, server_ws_id: u32, event: WsEvent) {
    use zeroship_runtime::websocket_native::network as nw;
    let state = runtime.state();
    nw::push_event_pub(&state, server_ws_id, event);
}

/// Set up the per-WS event log on both halves of the pair so the test
/// can observe events without racing the pump's dispatch drain.
fn enable_event_logs(runtime: &Runtime, client_ws_id: u32, server_ws_id: u32) {
    use zeroship_runtime::websocket_native::network as nw;
    let state = runtime.state();
    for id in [client_ws_id, server_ws_id] {
        if let Some(ws) = nw::lookup_native_ws_state(&state, id) {
            ws.borrow_mut().enable_event_log();
        }
    }
}

/// Drain outgoing frames the SERVER sent (which arrive on the CLIENT
/// side of the pair's `event_log`). Returns text frames + the close
/// (if present). Reads from `event_log` (the cumulative capture log)
/// rather than the live `events` queue so we don't race the V8 pump's
/// dispatch drain.
fn drain_outgoing_with_close(
    runtime: &Runtime,
    client_ws_id: u32,
) -> (Vec<String>, Option<(u16, String)>) {
    use zeroship_runtime::websocket_native::network as nw;

    let state = runtime.state();
    let mut text = Vec::new();
    let mut close = None;
    if let Some(ws) = nw::lookup_native_ws_state(&state, client_ws_id) {
        let mut s = ws.borrow_mut();
        if let Some(log) = s.event_log.take() {
            for ev in log {
                match ev {
                    WsEvent::MessageText(t) => text.push(t),
                    WsEvent::Close { code, reason, .. } => close = Some((code, reason)),
                    _ => {}
                }
            }
            // Re-arm the log so subsequent events are still captured.
            s.event_log = Some(Vec::new());
        }
    }
    (text, close)
}

/// Returns true once the client side has seen a Close event from the
/// server (i.e. the server-side WS issued `close()`).
fn client_saw_close(runtime: &Runtime, client_ws_id: u32) -> bool {
    use zeroship_runtime::websocket_native::network as nw;
    let state = runtime.state();
    let Some(ws) = nw::lookup_native_ws_state(&state, client_ws_id) else {
        return true;
    };
    let s = ws.borrow();
    s.event_log
        .as_ref()
        .map(|log| log.iter().any(|e| matches!(e, WsEvent::Close { .. })))
        .unwrap_or(false)
}

/// True once the client side has seen at least one `data` text frame.
fn client_saw_data_frame(runtime: &Runtime, client_ws_id: u32) -> bool {
    use zeroship_runtime::websocket_native::network as nw;
    let state = runtime.state();
    let Some(ws) = nw::lookup_native_ws_state(&state, client_ws_id) else {
        return false;
    };
    let s = ws.borrow();
    s.event_log
        .as_ref()
        .map(|log| {
            log.iter().any(|e| match e {
                WsEvent::MessageText(t) => t.contains("\"t\":\"data\""),
                _ => false,
            })
        })
        .unwrap_or(false)
}

fn global_bool(runtime: &Runtime, name: &str) -> bool {
    runtime.with_scope(|scope| {
        let global = scope.get_current_context().global(scope);
        let key = v8::String::new(scope, name).unwrap();
        global
            .get(scope, key.into())
            .is_some_and(|value| value.is_true())
    })
}

fn global_u32(runtime: &Runtime, name: &str) -> u32 {
    runtime.with_scope(|scope| {
        let global = scope.get_current_context().global(scope);
        let key = v8::String::new(scope, name).unwrap();
        global
            .get(scope, key.into())
            .and_then(|value| value.uint32_value(scope))
            .unwrap_or_default()
    })
}

/// Run the runtime's pump until the predicate fires (or timeout).
async fn pump_until<F: FnMut() -> bool>(runtime: &Runtime, mut predicate: F) {
    let deadline = std::time::Instant::now() + Duration::from_secs(2);
    while std::time::Instant::now() < deadline {
        if predicate() {
            return;
        }
        runtime.notify_pump();
        compio::time::sleep(Duration::from_millis(2)).await;
    }
}

// ── Tests ───────────────────────────────────────────────────────────────

/// Happy path: a 3-yield generator emits 3 data frames + end frame, then
/// the socket closes 1000.
#[test]
fn subscription_runs_async_gen_and_emits_frames() {
    init_v8();
    let modules = vec![ModuleEntry {
        specifier: "index.js".into(),
        source: r#"
            async function* sub() {
                yield { tick: 0 };
                yield { tick: 1 };
                yield { tick: 2 };
            }
            export default {
                rpc: {sub},
            };
        "#
        .into(),
    }];
    let runtime = Runtime::builder().modules(modules).build();
    let client_id = upgrade(&runtime);
    let server_ws_id = server_id(client_id);

    compio::runtime::Runtime::new().unwrap().block_on(async {
        runtime.start_pump();
        inject_server_message(&runtime, server_ws_id, r#"{"t":"hello","input":null}"#);

        pump_until(&runtime, || client_saw_close(&runtime, client_id)).await;

        let (frames, close) = drain_outgoing_with_close(&runtime, client_id);
        let data_count = frames
            .iter()
            .filter(|f| f.contains("\"t\":\"data\""))
            .count();
        assert_eq!(data_count, 3, "expected 3 data frames, got: {frames:#?}");
        assert!(
            frames.iter().any(|f| f.contains("\"t\":\"end\"")),
            "expected end frame, got: {frames:#?}"
        );
        let (code, _) = close.expect("expected close frame");
        assert_eq!(code, 1000, "expected normal close, got {code}");
    });
}

#[test]
fn subscription_waits_for_transport_drain_before_next_pull() {
    init_v8();
    let modules = vec![ModuleEntry {
        specifier: "index.js".into(),
        source: r#"
            globalThis.__subscriptionPulls = 0;
            async function* sub() {
                while (true) {
                    globalThis.__subscriptionPulls += 1;
                    yield globalThis.__subscriptionPulls;
                }
            }
            export default { rpc: {sub} };
        "#
        .into(),
    }];
    let runtime = Runtime::builder().modules(modules).build();
    let client_id = upgrade(&runtime);
    let server_ws_id = server_id(client_id);
    let (tx, mut rx) = futures::channel::mpsc::unbounded();
    {
        use zeroship_runtime::websocket_native::network as nw;
        let state = runtime.state();
        nw::lookup_native_ws_state(&state, client_id)
            .expect("client socket missing")
            .borrow_mut()
            .kernel_outbound = Some(tx);
    }

    compio::runtime::Runtime::new().unwrap().block_on(async {
        runtime.start_pump();
        inject_server_message(&runtime, server_ws_id, r#"{"t":"hello","input":null}"#);

        let first = compio::time::timeout(Duration::from_secs(2), rx.next())
            .await
            .expect("first transport frame timed out")
            .expect("transport closed before first frame");
        assert_eq!(global_u32(&runtime, "__subscriptionPulls"), 1);

        compio::time::sleep(Duration::from_millis(20)).await;
        assert_eq!(
            global_u32(&runtime, "__subscriptionPulls"),
            1,
            "producer advanced before the transport completed its write"
        );

        let (event, completion) = first.split();
        assert!(matches!(event, WsEvent::MessageText(ref text) if text.contains("\"t\":\"data\"")));
        completion.complete(&runtime.state());

        let second = compio::time::timeout(Duration::from_secs(2), rx.next())
            .await
            .expect("second transport frame timed out")
            .expect("transport closed before second frame");
        assert_eq!(global_u32(&runtime, "__subscriptionPulls"), 2);
        let (event, _) = second.split();
        assert!(matches!(event, WsEvent::MessageText(ref text) if text.contains("\"value\":2")));

        inject_server_close(&runtime, server_ws_id, 1000, "done");
    });
}

#[test]
fn subscription_uses_native_string_lookup_validation_and_context() {
    init_v8();
    let modules = vec![ModuleEntry {
        specifier: "index.js".into(),
        source: r#"
            globalThis.__zsDispatch = () => { throw new Error("forged dispatcher ran"); };
            async function* internalName(input, ctx) {
                yield {
                    validated: input.validated,
                    context: typeof ctx.requestId === "string",
                };
            }
            internalName.config = {
                kind: "subscription",
                input: {
                    parse(value) { return { validated: value.raw === true }; },
                },
            };
            export default {
                rpc: {sub: internalName},
            };
        "#
        .into(),
    }];
    let runtime = Runtime::builder().modules(modules).build();
    let client_id = upgrade(&runtime);
    let server_ws_id = server_id(client_id);

    compio::runtime::Runtime::new().unwrap().block_on(async {
        runtime.start_pump();
        inject_server_message(
            &runtime,
            server_ws_id,
            r#"{"t":"hello","input":{"raw":true}}"#,
        );

        pump_until(&runtime, || client_saw_close(&runtime, client_id)).await;

        let (frames, close) = drain_outgoing_with_close(&runtime, client_id);
        let data = frames
            .iter()
            .find(|frame| frame.contains("\"t\":\"data\""))
            .expect("data frame missing");
        assert!(
            data.contains("\"validated\":true"),
            "validator result missing: {data}"
        );
        assert!(
            data.contains("\"context\":true"),
            "RPC context missing: {data}"
        );
        assert_eq!(close.expect("close frame missing").0, 1000);
    });
}

#[test]
fn subscription_resolves_lazy_procedure_records() {
    init_v8();
    let modules = vec![ModuleEntry {
        specifier: "index.js".into(),
        source: r#"
            async function* loaded() { yield "loaded"; }
            export default {
                rpc: {
                    sub: {
                        async load() {
                            await Promise.resolve();
                            return loaded;
                        },
                    },
                },
            };
        "#
        .into(),
    }];
    let runtime = Runtime::builder().modules(modules).build();
    let client_id = upgrade(&runtime);
    let server_ws_id = server_id(client_id);

    compio::runtime::Runtime::new().unwrap().block_on(async {
        runtime.start_pump();
        inject_server_message(&runtime, server_ws_id, r#"{"t":"hello","input":null}"#);

        pump_until(&runtime, || client_saw_close(&runtime, client_id)).await;

        let (frames, close) = drain_outgoing_with_close(&runtime, client_id);
        assert!(
            frames
                .iter()
                .any(|frame| frame.contains("\"value\":\"loaded\"")),
            "lazy procedure output missing: {frames:#?}",
        );
        assert_eq!(close.expect("close frame missing").0, 1000);
    });
}

/// Mid-stream throw produces an `{"t":"error",...}` frame followed by
/// close 1011.
#[test]
fn subscription_emits_error_envelope_on_throw() {
    init_v8();
    let modules = vec![ModuleEntry {
        specifier: "index.js".into(),
        source: r#"
            async function* sub() {
                yield { tick: 0 };
                const err = new Error("kaboom");
                err.code = "INTERNAL";
                err.details = { hint: "demo" };
                throw err;
            }
            export default {
                rpc: {sub},
            };
        "#
        .into(),
    }];
    let runtime = Runtime::builder().modules(modules).build();
    let client_id = upgrade(&runtime);
    let server_ws_id = server_id(client_id);

    compio::runtime::Runtime::new().unwrap().block_on(async {
        runtime.start_pump();
        inject_server_message(&runtime, server_ws_id, r#"{"t":"hello","input":null}"#);

        pump_until(&runtime, || client_saw_close(&runtime, client_id)).await;

        let (frames, close) = drain_outgoing_with_close(&runtime, client_id);

        let data_count = frames
            .iter()
            .filter(|f| f.contains("\"t\":\"data\""))
            .count();
        let error = frames.iter().find(|f| f.contains("\"t\":\"error\""));
        assert_eq!(
            data_count, 1,
            "expected 1 data frame before error, got: {frames:#?}"
        );
        let err = error.expect("error frame missing");
        assert!(
            err.contains("\"code\":\"INTERNAL\""),
            "error envelope missing code: {err}"
        );
        assert!(
            err.contains("\"message\":\"kaboom\""),
            "error envelope missing message: {err}"
        );
        let (code, _) = close.expect("close frame missing");
        assert_eq!(code, 1011, "expected error close 1011, got {code}");
    });
}

/// A handler that returns a non-iterator emits an error envelope and closes.
#[test]
fn subscription_rejects_non_iterator_handler() {
    init_v8();
    let modules = vec![ModuleEntry {
        specifier: "index.js".into(),
        source: r#"
            async function sub() {
                return { tick: 0 };
            }
            export default {
                rpc: {sub},
            };
        "#
        .into(),
    }];
    let runtime = Runtime::builder().modules(modules).build();
    let client_id = upgrade(&runtime);
    let server_ws_id = server_id(client_id);

    compio::runtime::Runtime::new().unwrap().block_on(async {
        runtime.start_pump();
        inject_server_message(&runtime, server_ws_id, r#"{"t":"hello","input":null}"#);

        pump_until(&runtime, || client_saw_close(&runtime, client_id)).await;

        let (frames, close) = drain_outgoing_with_close(&runtime, client_id);
        assert!(
            frames.iter().any(|f| f.contains("\"t\":\"error\"")),
            "expected error frame, got: {frames:#?}"
        );
        let (code, _) = close.expect("close frame missing");
        assert_eq!(code, 1011);
    });
}

/// Client closes mid-stream → handler's generator return() is invoked,
/// no more data frames are sent.
#[test]
fn subscription_stops_when_client_closes() {
    init_v8();
    let modules = vec![ModuleEntry {
        specifier: "index.js".into(),
        source: r#"
            globalThis.__cleanupRan = false;
            async function* sub() {
                try {
                    for (let i = 0; ; i++) {
                        yield { tick: i };
                        await new Promise((r) => setTimeout(r, 50));
                    }
                } finally {
                    globalThis.__cleanupRan = true;
                }
            }
            export default {
                rpc: {sub},
            };
        "#
        .into(),
    }];
    let runtime = Runtime::builder().modules(modules).build();
    let client_id = upgrade(&runtime);
    let server_ws_id = server_id(client_id);

    compio::runtime::Runtime::new().unwrap().block_on(async {
        runtime.start_pump();
        inject_server_message(&runtime, server_ws_id, r#"{"t":"hello","input":null}"#);

        // Wait for first data frame to surface on the client side.
        pump_until(&runtime, || client_saw_data_frame(&runtime, client_id)).await;

        // Now close the client side — drives the server's `_onClose`.
        inject_server_close(&runtime, server_ws_id, 1000, "test-close");

        pump_until(&runtime, || global_bool(&runtime, "__cleanupRan")).await;
        assert!(global_bool(&runtime, "__cleanupRan"));
    });
}

/// Hello frame with malformed JSON closes 4400.
#[test]
fn subscription_rejects_malformed_hello() {
    init_v8();
    let modules = vec![ModuleEntry {
        specifier: "index.js".into(),
        source: r#"
            async function* sub() { yield 1; }
            export default {
                rpc: {sub},
            };
        "#
        .into(),
    }];
    let runtime = Runtime::builder().modules(modules).build();
    let client_id = upgrade(&runtime);
    let server_ws_id = server_id(client_id);

    compio::runtime::Runtime::new().unwrap().block_on(async {
        runtime.start_pump();
        inject_server_message(&runtime, server_ws_id, "not-json{");

        pump_until(&runtime, || client_saw_close(&runtime, client_id)).await;

        let (_frames, close) = drain_outgoing_with_close(&runtime, client_id);
        let (code, _) = close.expect("close frame missing");
        assert_eq!(code, 4400, "expected 4400 BAD_REQUEST, got {code}");
    });
}

// Eviction runs no creator JavaScript on a halted isolate
// ---------------------------------------------------------------------------

use std::cell::Cell;
use std::sync::Arc;
use zeroship_runtime::{NativePlugin, NativeRegistrar};

thread_local! {
    /// How many times an aborted subscription's listener reported running,
    /// through `env.probe.ran()`, on this thread.
    static ABORT_LISTENER_RAN: Cell<u32> = const { Cell::new(0) };
}

fn abort_listener_ran(_scope: &mut v8::PinScope, _args: v8::FunctionCallbackArguments, _rv: v8::ReturnValue) {
    ABORT_LISTENER_RAN.with(|c| c.set(c.get() + 1));
}

/// `env.probe.ran()`, the recorder a subscription's abort listener calls so a
/// test can see it run without reading V8 state on a quarantined isolate.
struct AbortProbe;

impl NativePlugin for AbortProbe {
    fn namespace(&self) -> &'static str {
        "probe"
    }

    fn register(&self, r: &mut NativeRegistrar) {
        r.add("ran", abort_listener_ran);
    }
}

/// The app: `sub` registers an abort listener that reports through
/// `env.probe.ran()` and then stays open; `fetch` allocates past the cap from
/// a timer, so a dispatch to it reaches the heap cap and stops the isolate.
const ABORT_SUB_APP: &str = r#"
    import { currentSignal, env } from "zeroship";
    async function* sub() {
        currentSignal().addEventListener("abort", () => { env.probe.ran(); });
        // Yield once so a test can see the listener is attached before it
        // evicts, then stay open.
        yield 0;
        await new Promise(() => {});
    }
    export default {
        fetch() {
            return new Promise(() => setTimeout(() => new Array(20_000_000).fill(1.5), 0));
        },
        rpc: { sub },
    };
"#;

fn abort_sub_runtime(heap_limit_mb: Option<u32>) -> Runtime {
    init_v8();
    let mut builder = Runtime::builder()
        .modules(vec![ModuleEntry { specifier: "index.js".into(), source: ABORT_SUB_APP.into() }])
        .plugins(vec![Arc::new(AbortProbe) as Arc<dyn NativePlugin>])
        .idle_gc_after_ms(0);
    if let Some(mb) = heap_limit_mb {
        builder = builder.heap_limit_mb(mb);
    }
    builder.build()
}

/// Open the `sub` subscription and drive the pump until its abort listener is
/// registered (one in-flight signal on the runtime).
async fn open_sub_with_abort_listener(runtime: &Runtime) -> u32 {
    let client_id = upgrade(runtime);
    let server_ws_id = server_id(client_id);
    runtime.start_pump();
    inject_server_message(runtime, server_ws_id, r#"{"t":"hello","input":null}"#);
    // The generator yields once only after it has attached its abort listener,
    // so a data frame means the listener is wired; registry length alone is
    // set at open, before the generator runs.
    pump_until(runtime, || client_saw_data_frame(runtime, client_id)).await;
    assert!(
        client_saw_data_frame(runtime, client_id),
        "the premise: the subscription yielded, so its abort listener is attached",
    );
    assert_eq!(runtime.abort_registry_len(), 1, "the premise: the subscription's signal is registered");
    client_id
}

/// The control: on a LIVE isolate, eviction aborts the subscription's signal
/// and the creator's abort listener runs. This is the behaviour the halted
/// case below must NOT reproduce, so it proves the listener is really wired.
#[test]
fn eviction_runs_the_abort_listener_on_a_live_isolate() {
    let runtime = abort_sub_runtime(None);
    compio::runtime::Runtime::new().unwrap().block_on(async {
        let _client_id = open_sub_with_abort_listener(&runtime).await;
        let before = ABORT_LISTENER_RAN.with(Cell::get);
        runtime.entered_for_eviction();
        compio::time::sleep(Duration::ZERO).await;
        assert_eq!(
            ABORT_LISTENER_RAN.with(Cell::get),
            before + 1,
            "eviction of a live isolate runs the subscription's abort listener",
        );
        assert_eq!(
            runtime.abort_registry_len(),
            0,
            "eviction of a live isolate drains the signal it aborted",
        );
    });
}

/// An isolate stopped at its heap cap runs no creator JavaScript when it is
/// later evicted: `entered_for_eviction` would otherwise abort the open
/// subscription's signal and run the creator's abort listener on the heap the
/// cap's grants raised. The stop leaves the subscription registered (it drains
/// only pending requests), so the registry is non-empty at eviction.
#[test]
fn eviction_runs_no_abort_listener_on_a_heap_stopped_isolate() {
    let runtime = abort_sub_runtime(Some(64));
    compio::runtime::Runtime::new().unwrap().block_on(async {
        let _client_id = open_sub_with_abort_listener(&runtime).await;

        // Reach the heap cap through a fetch; the stop quarantines the isolate.
        let env = EnvSnapshot::empty();
        let ctx = RequestCtx::new(CancelFlag::new());
        let outcome = runtime.call_fetch_handler("GET", "http://localhost/", &[], "", &env, ctx);
        let FetchOutcome::Pending { rx, .. } = outcome else {
            panic!("the allocation runs on a timer, so the dispatch is pending");
        };
        let settled = compio::time::timeout(Duration::from_secs(10), rx.recv())
            .await
            .expect("the dispatch is answered, not stranded");
        match &settled {
            Err(e) => assert_eq!(
                e.message, "memory limit exceeded",
                "the premise: the heap cap stops the isolate",
            ),
            Ok(_) => panic!("the premise: the heap cap stops the isolate, but it answered"),
        }
        assert!(runtime.is_quarantined(), "the premise: the isolate is quarantined");
        assert_eq!(
            runtime.abort_registry_len(),
            1,
            "the premise: the subscription's signal survives the stop",
        );

        let before = ABORT_LISTENER_RAN.with(Cell::get);
        runtime.entered_for_eviction();
        compio::time::sleep(Duration::ZERO).await;
        assert_eq!(
            ABORT_LISTENER_RAN.with(Cell::get),
            before,
            "eviction ran the subscription's abort listener on a halted isolate",
        );
        // The gate skips the abort entirely, so the signal is never even
        // touched: no `build_abort_error` allocation, no dispatch. A drained
        // registry would mean eviction entered V8 on the halted isolate.
        assert_eq!(
            runtime.abort_registry_len(),
            1,
            "eviction aborted the signal on a halted isolate instead of skipping it",
        );
    });
}
