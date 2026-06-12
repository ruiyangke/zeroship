//! End-to-end streaming test for `env.storage` — drives JS through the REAL
//! V8 runtime + compio event loop down through the native streaming
//! callbacks and the `response_forwarder` / `StreamWriter` bridges to a live
//! `LocalFs` backend, then back out as a JS `ReadableStream`.
//!
//! ## What this covers that `backend_parity.rs` does NOT
//!
//! The parity test drives the `Backend` trait directly. This test exercises
//! the parts that only exist in V8:
//!
//!     JS `env.storage.putStream(bucket, key, ReadableStream, ct)`
//!       → response_forwarder.begin_forward_stream (getReader + read loop)
//!       → StreamWriter → StreamReader → Backend::put_stream
//!
//!     JS `env.storage.getStream(bucket, key)` + `readChunk(id)` loop
//!       → Backend::get_stream → per-isolate registry → ResolveValue::Bytes
//!       → JS reassembles the object
//!
//! The handler self-asserts a multi-chunk upload → download round-trip
//! (byte-compare) and returns `{ok:true}`; the Rust side asserts 200+ok.
//!
//! Harness mirrors `crates/plugin-kv/tests/e2e_runtime.rs`.

#![allow(clippy::future_not_send)]

use std::collections::HashMap;
use std::sync::Arc;
use std::time::Duration;

use zeroship_runtime::channel::CancelFlag;
use zeroship_runtime::plugin::NativePlugin;
use zeroship_runtime::{
    init_v8, EnvSnapshot, FetchOutcome, ModuleEntry, RequestCtx, Runtime, SettledFetch,
};

use zeroship_plugin_storage::StoragePlugin;

// The app exercises the native streaming surface directly (NOT the SDK),
// then self-asserts. A multi-chunk ReadableStream forces the upload read
// loop through several `read()` reactions; the download is reassembled by
// looping `readChunk` until it returns `undefined`.
const STORAGE_STREAM_E2E_APP: &str = r#"
export default {
    async fetch(request, env, ctx) {
        const s = env.storage;
        const BUCKET = "uploads";
        const KEY = "stream/obj.bin";

        function fail(step, got, want) {
            const e = new Error("step failed: " + step);
            e.zsStep = step; e.zsGot = got; e.zsWant = want;
            throw e;
        }
        const trace = [];
        function truthy(step, got) { trace.push(step); if (!got) fail(step, got, "truthy"); }
        function eq(step, got, want) { trace.push(step); if (got !== want) fail(step, got, want); }

        // Build a deterministic multi-chunk payload (~700 KiB across 7 chunks).
        const CHUNK = 100 * 1024;
        const N = 7;
        const total = CHUNK * N;
        const expected = new Uint8Array(total);
        const chunks = [];
        for (let c = 0; c < N; c++) {
            const buf = new Uint8Array(CHUNK);
            for (let i = 0; i < CHUNK; i++) {
                const v = (c * 31 + (i % 251)) & 0xff;
                buf[i] = v;
                expected[c * CHUNK + i] = v;
            }
            chunks.push(buf);
        }

        try {
            // ---- streaming PUT via a ReadableStream ----
            const upload = new ReadableStream({
                start(controller) {
                    for (const ch of chunks) controller.enqueue(ch);
                    controller.close();
                },
            });
            const putRaw = await s.putStream(BUCKET, KEY, upload, "application/octet-stream");
            const put = JSON.parse(putRaw);
            eq("put.size", put.size, total);
            eq("put.key", put.key, KEY);

            // ---- streaming GET: open + reassemble via readChunk ----
            const handleRaw = await s.getStream(BUCKET, KEY);
            truthy("get.handle", handleRaw && handleRaw !== "null");
            const handle = JSON.parse(handleRaw);
            eq("get.size", handle.size, total);
            truthy("get.streamId", typeof handle.streamId === "number");

            const got = new Uint8Array(total);
            let off = 0;
            for (;;) {
                const chunk = await s.readChunk(handle.streamId);
                if (chunk === undefined || chunk === null) break;
                truthy("get.chunk.isU8", chunk instanceof Uint8Array);
                got.set(chunk, off);
                off += chunk.length;
            }
            eq("get.totalRead", off, total);

            // ---- byte-compare ----
            let mismatch = -1;
            for (let i = 0; i < total; i++) {
                if (got[i] !== expected[i]) { mismatch = i; break; }
            }
            eq("bytes.match", mismatch, -1);

            // ---- missing object → null handle ----
            const missing = await s.getStream(BUCKET, "stream/nope.bin");
            eq("get.missing", missing, "null");

            // ---- cancel a fresh stream mid-read ----
            const h2raw = await s.getStream(BUCKET, KEY);
            const h2 = JSON.parse(h2raw);
            const first = await s.readChunk(h2.streamId);
            truthy("cancel.firstChunk", first instanceof Uint8Array);
            await s.cancelStream(h2.streamId);
            // A readChunk after cancel must resolve undefined (EOF), not hang/throw.
            const afterCancel = await s.readChunk(h2.streamId);
            eq("cancel.afterEof", afterCancel, undefined);

            return Response.json({ ok: true, trace });
        } catch (e) {
            return Response.json({
                ok: false,
                step: e && e.zsStep,
                got: e && e.zsGot,
                want: e && e.zsWant,
                message: (e && e.message) || String(e),
            }, { status: 500 });
        }
    },
};
"#;

fn module(source: &str) -> Vec<ModuleEntry> {
    vec![ModuleEntry {
        specifier: "index.js".into(),
        source: source.into(),
    }]
}

fn run_app(app: &'static str) -> (u16, String) {
    compio::runtime::Runtime::new().unwrap().block_on(async move {
        init_v8();

        let dir = std::env::temp_dir().join(format!("zs-storage-e2e-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);

        let mut env_vars = HashMap::new();
        env_vars.insert("APP_ID".to_string(), "e2e_app".to_string());

        let plugin: Arc<dyn NativePlugin> = Arc::new(StoragePlugin::local(&dir));

        let runtime = Runtime::builder()
            .modules(module(app))
            .env_vars(env_vars)
            .plugins(vec![plugin])
            .build();
        runtime.start_pump();

        let env = EnvSnapshot::empty();
        let ctx = RequestCtx::new(CancelFlag::new());
        let outcome = runtime.call_fetch_handler("GET", "http://localhost/", &[], "", &env, ctx);

        let result = match outcome {
            FetchOutcome::Response { status, body, .. } => (status, body),
            FetchOutcome::Pending { rx, cancel: _ } => {
                let settled = compio::time::timeout(Duration::from_secs(30), rx.recv())
                    .await
                    .expect("storage e2e: fetch pending timed out")
                    .expect("storage e2e: pending delivered DispatchError");
                match settled {
                    SettledFetch::Response { status, body, .. } => (status, body),
                    other => {
                        let name = match other {
                            SettledFetch::Stream { .. } => "Stream",
                            SettledFetch::WebSocketUpgrade { .. } => "WebSocketUpgrade",
                            SettledFetch::Response { .. } => unreachable!(),
                        };
                        panic!("storage e2e: expected SettledFetch::Response, got {name}");
                    }
                }
            }
            FetchOutcome::Stream { .. } => panic!("storage e2e: unexpected Stream outcome"),
            FetchOutcome::WebSocketUpgrade { .. } => {
                panic!("storage e2e: unexpected WebSocketUpgrade outcome")
            }
        };

        let _ = std::fs::remove_dir_all(&dir);
        result
    })
}

#[test]
fn e2e_storage_streaming_localfs() {
    let (status, body) = run_app(STORAGE_STREAM_E2E_APP);
    assert_eq!(status, 200, "storage e2e returned non-200; body: {body}");
    assert!(
        body.contains(r#""ok":true"#),
        "storage e2e reported failure; body: {body}"
    );
}
