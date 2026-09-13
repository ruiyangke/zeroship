//! Native RPC cancellation follows handler and iterator continuations.

use std::time::Duration;

use zeroship_runtime::channel::{CancelFlag, StreamReader};
use zeroship_runtime::runtime::Runtime;
use zeroship_runtime::{EnvSnapshot, FetchOutcome, ModuleEntry, RequestCtx, SettledFetch};

fn runtime(source: String, deadline: Option<Duration>) -> Runtime {
    let mut builder = Runtime::builder().modules(vec![ModuleEntry {
        specifier: "index.js".into(),
        source,
    }]);
    if let Some(deadline) = deadline {
        builder = builder.wall_timeout(deadline);
    }
    builder.build()
}

fn request(runtime: &Runtime, name: &str, cancel: CancelFlag) -> FetchOutcome {
    runtime.call_fetch_handler(
        "POST",
        &format!("http://localhost/__zeroship/v1/{name}"),
        &[("content-type".into(), "application/json".into())],
        "{}",
        &EnvSnapshot::empty(),
        RequestCtx::new(cancel),
    )
}

async fn settle(outcome: FetchOutcome) -> SettledFetch {
    match outcome {
        FetchOutcome::Stream {
            status,
            headers,
            body_reader,
            logs,
        } => SettledFetch::Stream {
            status,
            headers,
            body_reader,
            logs,
        },
        FetchOutcome::Response {
            status,
            headers,
            body,
            logs,
        } => SettledFetch::Response {
            status,
            headers,
            body,
            logs,
        },
        FetchOutcome::Pending { rx, .. } => {
            compio::time::timeout(Duration::from_secs(5), rx.recv())
                .await
                .expect("request must settle")
                .expect("request transport failed")
        }
        _ => panic!("unexpected websocket"),
    }
}

async fn inspect(runtime: &Runtime) -> serde_json::Value {
    let SettledFetch::Response { status, body, .. } =
        settle(request(runtime, "inspect", CancelFlag::new())).await
    else {
        panic!("inspection must return JSON");
    };
    assert_eq!(status, 200, "{}", String::from_utf8_lossy(&body));
    serde_json::from_slice::<serde_json::Value>(&body).unwrap()["json"].clone()
}

async fn wait_for(
    runtime: &Runtime,
    predicate: impl Fn(&serde_json::Value) -> bool,
) -> serde_json::Value {
    compio::time::timeout(Duration::from_secs(5), async {
        loop {
            let value = inspect(runtime).await;
            if predicate(&value) {
                return value;
            }
            compio::time::sleep(Duration::from_millis(1)).await;
        }
    })
    .await
    .expect("iterator state must advance")
}

async fn read_body(reader: StreamReader) -> String {
    compio::time::timeout(Duration::from_secs(5), async {
        let mut bytes = Vec::new();
        loop {
            while let Some(chunk) = reader.pop() {
                bytes.extend_from_slice(&chunk);
            }
            if reader.is_done() {
                break;
            }
            reader.wait_for_data().await;
        }
        assert!(!reader.is_overflow());
        assert!(reader.error().is_none());
        String::from_utf8(bytes).unwrap()
    })
    .await
    .expect("stream must finish")
}

fn stalled_stream(promised: bool) -> String {
    format!(
        r#"
        import {{ getRequestContext, currentSignal }} from 'zeroship';
        const state = {{pulls:0, returned:0, closed:false, aborted:0, same:true}};
        let late;
        const make = (_, ctx) => {{
            ctx.signal.addEventListener('abort', () => {{
                state.aborted++;
                state.reason = ctx.signal.reason.name;
                state.same &&= getRequestContext() === ctx;
            }});
            return {{
                [Symbol.asyncIterator]() {{ return this; }},
                next() {{
                    state.pulls++;
                    if (state.pulls === 1) return {{done:false, value:'first'}};
                    return new Promise(resolve => {{ late = resolve; }});
                }},
                async return() {{
                    state.returned++;
                    state.same &&= getRequestContext() === ctx;
                    state.sawAbort = ctx.signal.aborted;
                    await new Promise(resolve => setTimeout(resolve, 1));
                    state.same &&= getRequestContext() === ctx && currentSignal() === ctx.signal;
                    state.closed = true;
                    return {{done:true}};
                }},
            }};
        }};
        const stream = {handler};
        stream.config = {{kind:'stream'}};
        export default {{rpc:{{stream, inspect:()=>state, late:()=>{{
            late?.({{done:false, value:'must not reach the wire'}});
            return true;
        }}}}}};
    "#,
        handler = if promised {
            "async (...args) => { await new Promise(r=>setTimeout(r,1)); return make(...args); }"
        } else {
            "make"
        }
    )
}

#[derive(Clone, Copy)]
enum CancelSource {
    Flag,
    Deadline,
    Reader,
}

#[compio::test]
async fn cancellation_aborts_stream_context_and_returns_the_retained_iterator() {
    for promised in [false, true] {
        for source in [
            CancelSource::Flag,
            CancelSource::Deadline,
            CancelSource::Reader,
        ] {
            let deadline =
                matches!(source, CancelSource::Deadline).then_some(Duration::from_millis(100));
            let runtime = runtime(stalled_stream(promised), deadline);
            runtime.start_pump();
            let cancel = CancelFlag::new();
            let SettledFetch::Stream { body_reader, .. } =
                settle(request(&runtime, "stream", cancel.clone())).await
            else {
                panic!("expected stream");
            };
            let mut reader = Some(body_reader);
            let first = compio::time::timeout(Duration::from_secs(5), async {
                loop {
                    if let Some(chunk) = reader.as_ref().unwrap().pop() {
                        return chunk;
                    }
                    reader.as_ref().unwrap().wait_for_data().await;
                }
            })
            .await
            .unwrap();
            assert_eq!(first, b"0:\"first\"\n");
            let pending = wait_for(&runtime, |state| state["pulls"] == 2).await;
            assert_eq!(pending["closed"], false);
            match source {
                CancelSource::Flag => cancel.cancel(),
                CancelSource::Reader => drop(reader.take()),
                CancelSource::Deadline => {}
            }
            if let Some(reader) = reader.take() {
                let body = read_body(reader).await;
                let frames: Vec<_> = body.lines().collect();
                assert_eq!(frames.len(), 2, "{body}");
                assert_eq!(frames[1], "d:{}");
                let error: serde_json::Value =
                    serde_json::from_str(frames[0].strip_prefix("e:").unwrap()).unwrap();
                assert_eq!(
                    error["code"],
                    if matches!(source, CancelSource::Deadline) {
                        "TIMEOUT"
                    } else {
                        "CANCELLED"
                    }
                );
            }
            let state = wait_for(&runtime, |state| state["closed"] == true).await;
            assert_eq!(state["returned"], 1);
            assert_eq!(state["aborted"], 1);
            assert_eq!(state["sawAbort"], true);
            assert_eq!(state["same"], true);
            assert_eq!(
                state["reason"],
                if matches!(source, CancelSource::Deadline) {
                    "TimeoutError"
                } else {
                    "AbortError"
                }
            );
            assert!(cancel.is_cancelled());
            let _ = settle(request(&runtime, "late", CancelFlag::new())).await;
            let state = inspect(&runtime).await;
            assert_eq!(state["pulls"], 2);
            assert_eq!(state["returned"], 1);
            runtime.with_scope(|scope| {
                let state = scope
                    .get_slot::<zeroship_runtime::state::SharedState>()
                    .unwrap();
                assert!(state.borrow().response_forwarders.is_empty());
            });
        }
    }
}

#[compio::test]
async fn pending_rpc_deadline_aborts_its_signal_without_waiting_for_native_io() {
    let runtime = runtime(
        r#"
        import {getRequestContext} from 'zeroship';
        const state = {aborted:false, same:false};
        export default {rpc:{
            pending: (_, ctx) => {
                ctx.signal.addEventListener('abort', () => {
                    state.aborted = ctx.signal.aborted;
                    state.reason = ctx.signal.reason.name;
                    state.same = getRequestContext() === ctx;
                });
                return new Promise(()=>{});
            },
            inspect:()=>state,
        }};
    "#
        .into(),
        Some(Duration::from_millis(30)),
    );
    runtime.start_pump();
    let outcome = request(&runtime, "pending", CancelFlag::new());
    assert!(matches!(outcome, FetchOutcome::Pending { .. }));
    let SettledFetch::Response { status, body, .. } = settle(outcome).await else {
        panic!("expected timeout envelope");
    };
    assert_eq!(status, 504);
    let value: serde_json::Value = serde_json::from_slice(&body).unwrap();
    assert_eq!(value["code"], "TIMEOUT");
    assert_eq!(
        inspect(&runtime).await,
        serde_json::json!({
            "aborted":true, "same":true, "reason":"TimeoutError",
        })
    );
}

#[compio::test]
async fn iterator_completion_and_failure_run_return_in_the_original_context() {
    for failed in [false, true] {
        let source = format!(
            r#"
            import {{getRequestContext}} from 'zeroship';
            const state = {{returned:0, closed:false, same:false}};
            export default {{rpc:{{
                stream: (_, ctx) => ({{
                    [Symbol.asyncIterator]() {{return this;}},
                    next() {{ {step} }},
                    async return() {{
                        state.returned++;
                        await new Promise(r=>setTimeout(r,1));
                        state.same = getRequestContext() === ctx;
                        state.closed = true;
                        throw new Error('secondary cleanup failure');
                    }},
                }}),
                inspect:()=>state,
            }}}};
        "#,
            step = if failed {
                "throw new RpcError('INVALID_ARGUMENT', 'original failure');"
            } else {
                "return {done:true};"
            }
        );
        let runtime = runtime(source, None);
        runtime.start_pump();
        let SettledFetch::Stream { body_reader, .. } =
            settle(request(&runtime, "stream", CancelFlag::new())).await
        else {
            panic!("expected stream");
        };
        let body = read_body(body_reader).await;
        if failed {
            let error: serde_json::Value =
                serde_json::from_str(body.lines().next().unwrap().strip_prefix("e:").unwrap())
                    .unwrap();
            assert_eq!(error["message"], "original failure");
            assert_eq!(error["code"], "INVALID_ARGUMENT");
        } else {
            assert_eq!(body, "d:{}\n");
        }
        assert!(body.ends_with("d:{}\n"));
        let state = wait_for(&runtime, |state| state["closed"] == true).await;
        assert_eq!(state["returned"], 1);
        assert_eq!(state["same"], true);
    }
}

#[compio::test]
async fn overflowing_the_body_channel_cancels_the_original_iterator() {
    let (writer, _) = zeroship_runtime::channel::stream_buffer();
    let size = writer.cap() + 1;
    let source = format!(
        r#"
        const state = {{returned:0, aborted:false}};
        export default {{rpc:{{
            stream: (_,ctx) => ({{
                [Symbol.asyncIterator]() {{return this;}},
                next() {{return {{value:'x'.repeat({size}), done:false}};}},
                return() {{state.returned++; state.aborted=ctx.signal.aborted; return {{done:true}};}},
            }}),
            inspect:()=>state,
        }}}};
    "#
    );
    let runtime = runtime(source, None);
    runtime.start_pump();
    let SettledFetch::Stream { body_reader, .. } =
        settle(request(&runtime, "stream", CancelFlag::new())).await
    else {
        panic!("expected stream");
    };
    assert!(body_reader.is_overflow());
    let state = wait_for(&runtime, |state| state["returned"] == 1).await;
    assert_eq!(state["aborted"], true);
}

#[compio::test]
async fn cancelling_a_lazy_waiter_preserves_the_shared_load_and_other_request() {
    let runtime = runtime(
        r#"
        import {getRequestContext} from 'zeroship';
        const state = {loads:0, calls:0, detached:true};
        export default {rpc:{
            delayed: {async load() {
                state.loads++;
                state.detached &&= getRequestContext() === undefined;
                await new Promise(resolve=>setTimeout(resolve,20));
                state.detached &&= getRequestContext() === undefined;
                const handler = (_,ctx) => {
                    state.calls++;
                    return {same:ctx === getRequestContext(), aborted:ctx.signal.aborted};
                };
                Object.defineProperty(handler, 'config', {get() {
                    state.detached &&= getRequestContext() === undefined;
                    return {kind:'query'};
                }});
                return handler;
            }},
            inspect:()=>state,
        }};
    "#
        .into(),
        None,
    );
    runtime.start_pump();
    let cancel = CancelFlag::new();
    let first = request(&runtime, "delayed", cancel.clone());
    let second = request(&runtime, "delayed", CancelFlag::new());
    assert!(matches!(first, FetchOutcome::Pending { .. }));
    assert!(matches!(second, FetchOutcome::Pending { .. }));
    cancel.cancel();
    let SettledFetch::Response { status, body, .. } = settle(first).await else {
        panic!("expected cancellation");
    };
    assert_eq!(status, 499);
    assert_eq!(
        serde_json::from_slice::<serde_json::Value>(&body).unwrap()["code"],
        "CANCELLED"
    );
    let SettledFetch::Response { status, body, .. } = settle(second).await else {
        panic!("expected surviving request");
    };
    assert_eq!(status, 200);
    assert_eq!(
        serde_json::from_slice::<serde_json::Value>(&body).unwrap()["json"],
        serde_json::json!({"same":true,"aborted":false})
    );
    assert_eq!(
        inspect(&runtime).await,
        serde_json::json!({"loads":1,"calls":1,"detached":true})
    );
}

#[compio::test]
async fn creator_timeout_errors_keep_their_private_details_redacted() {
    let runtime = runtime(
        r#"
        export default {rpc:{fail:()=>{
            throw new RpcError('TIMEOUT', 'private creator message', {
                details:{secret:'private creator detail'}, retryable:false,
            });
        }}};
    "#
        .into(),
        None,
    );
    runtime.start_pump();
    let SettledFetch::Response { status, body, .. } =
        settle(request(&runtime, "fail", CancelFlag::new())).await
    else {
        panic!("expected error response");
    };
    assert_eq!(status, 504);
    let error: serde_json::Value = serde_json::from_slice(&body).unwrap();
    assert_eq!(error["message"], "internal error");
    assert!(error.get("code").is_none());
    assert!(error.get("details").is_none());
    assert!(error.get("retryable").is_none());
    assert!(!String::from_utf8(body).unwrap().contains("private creator"));
}

#[compio::test]
async fn rpc_response_body_cancellation_reaches_its_source_and_request_signal() {
    let runtime = runtime(
        r#"
        import {getRequestContext} from 'zeroship';
        const state = {cancelled:0, aborted:false, same:false};
        export default {rpc:{
            stream: (_,ctx) => new Response(new ReadableStream({
                start(controller) {controller.enqueue(new TextEncoder().encode('first'));},
                pull() {return new Promise(()=>{});},
                cancel(reason) {
                    state.cancelled++;
                    state.aborted=ctx.signal.aborted;
                    state.same=getRequestContext() === ctx;
                    state.reason=reason.name;
                },
            })),
            inspect:()=>state,
        }};
    "#
        .into(),
        None,
    );
    runtime.start_pump();
    let cancel = CancelFlag::new();
    let SettledFetch::Stream { body_reader, .. } =
        settle(request(&runtime, "stream", cancel.clone())).await
    else {
        panic!("expected response body");
    };
    cancel.cancel();
    compio::time::timeout(Duration::from_secs(5), async {
        while !body_reader.is_done() {
            body_reader.drain();
            body_reader.wait_for_data().await;
        }
    })
    .await
    .expect("cancelled response must finish");
    assert_eq!(body_reader.error().as_deref(), Some("Request cancelled"));
    assert_eq!(
        inspect(&runtime).await,
        serde_json::json!({
            "cancelled":1,"aborted":true,"same":true,"reason":"AbortError",
        })
    );
}

#[compio::test]
async fn a_paused_stream_expires_while_the_consumer_is_idle() {
    let (writer, _) = zeroship_runtime::channel::stream_buffer();
    let size = writer.cap() / 2;
    let source = format!(
        r#"
        const state = {{pulls:0, returned:0, reason:null}};
        export default {{rpc:{{
            stream: (_,ctx) => ({{
                [Symbol.asyncIterator]() {{return this;}},
                next() {{state.pulls++; return {{value:'x'.repeat({size}),done:false}};}},
                return() {{state.returned++; state.reason=ctx.signal.reason.name; return {{done:true}};}},
            }}),
            inspect:()=>state,
        }}}};
    "#
    );
    let runtime = runtime(source, Some(Duration::from_millis(30)));
    runtime.start_pump();
    let SettledFetch::Stream { body_reader, .. } =
        settle(request(&runtime, "stream", CancelFlag::new())).await
    else {
        panic!("expected stream");
    };
    compio::time::sleep(Duration::from_millis(100)).await;
    assert_eq!(
        inspect(&runtime).await,
        serde_json::json!({"pulls":1,"returned":1,"reason":"TimeoutError"})
    );
    let body = read_body(body_reader).await;
    let frames: Vec<_> = body.lines().collect();
    assert_eq!(frames.len(), 3);
    assert_eq!(frames[2], "d:{}");
    let error: serde_json::Value =
        serde_json::from_str(frames[1].strip_prefix("e:").unwrap()).unwrap();
    assert_eq!(error["code"], "TIMEOUT");
}

#[compio::test]
async fn cancellation_callbacks_can_settle_another_rpc_without_native_io() {
    for streaming in [false, true] {
        let source = format!(
            r#"
            let release;
            export default {{rpc:{{
                observer:()=>new Promise(resolve=>{{release=resolve;}}),
                cancelled:(_,ctx)=>{{
                    ctx.signal.addEventListener('abort',()=>release('observed cancellation'));
                    return {result};
                }},
            }}}};
        "#,
            result = if streaming {
                "{[Symbol.asyncIterator](){return this;}, next(){return new Promise(()=>{});}, return(){return {done:true};}}"
            } else {
                "new Promise(()=>{})"
            }
        );
        let runtime = runtime(source, None);
        runtime.start_pump();
        let observer = request(&runtime, "observer", CancelFlag::new());
        assert!(matches!(observer, FetchOutcome::Pending { .. }));
        let cancel = CancelFlag::new();
        let cancelled = request(&runtime, "cancelled", cancel.clone());
        cancel.cancel();
        let SettledFetch::Response { status, body, .. } = settle(observer).await else {
            panic!("observer must settle");
        };
        assert_eq!(status, 200);
        assert_eq!(
            serde_json::from_slice::<serde_json::Value>(&body).unwrap()["json"],
            "observed cancellation"
        );
        let _ = settle(cancelled).await;
    }
}
