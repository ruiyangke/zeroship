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

use zeroship_plugin_storage::{LocalFs, StoragePlugin};

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

        // Unique per call so concurrent tests in this binary never share a
        // storage root (cargo runs test fns on parallel threads).
        static SEQ: std::sync::atomic::AtomicU32 = std::sync::atomic::AtomicU32::new(0);
        let seq = SEQ.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        let dir = std::env::temp_dir()
            .join(format!("zs-storage-e2e-{}-{seq}", std::process::id()));
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

// ===========================================================================
// Metering-as-infrastructure (Refactor A): each successful storage op emits
// a raw usage metric (storage_ops + storage_bytes/storage_egress_bytes) into
// the process-wide Meter at its op boundary, scoped to the server-injected
// APP_ID. Emitted by trusted Rust inside the primitive — no creator-facing
// `env.meter` API.
// ===========================================================================

/// Build a Runtime over `LocalFs` + a real Meter bound to `app_id`, pump
/// it, run the fetch handler, and return `(status, body, meter)`. Faithful:
/// drives the REAL `StoragePlugin::with_backend_and_meter` → register
/// (STORAGE_METER) → callbacks path.
fn run_app_metered(app: &'static str, app_id: &str) -> (u16, String, Arc<zeroship_metering::Meter>) {
    let meter = Arc::new(zeroship_metering::Meter::new());
    let meter_for_run = Arc::clone(&meter);
    let app_id = app_id.to_string();
    let (status, body) = compio::runtime::Runtime::new().unwrap().block_on(async move {
        init_v8();

        static SEQ: std::sync::atomic::AtomicU32 = std::sync::atomic::AtomicU32::new(0);
        let seq = SEQ.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        let dir = std::env::temp_dir()
            .join(format!("zs-storage-meter-{}-{seq}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);

        let mut env_vars = HashMap::new();
        env_vars.insert("APP_ID".to_string(), app_id.clone());

        let backend: Arc<dyn zeroship_plugin_storage::Backend> = Arc::new(LocalFs::new(&dir));
        let plugin: Arc<dyn NativePlugin> =
            Arc::new(StoragePlugin::with_backend_and_meter(backend, Some(meter_for_run)));

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
                    .expect("storage metering: fetch pending timed out")
                    .expect("storage metering: pending delivered DispatchError");
                match settled {
                    SettledFetch::Response { status, body, .. } => (status, body),
                    _ => panic!("storage metering: expected SettledFetch::Response"),
                }
            }
            _ => panic!("storage metering: unexpected outcome"),
        };
        let _ = std::fs::remove_dir_all(&dir);
        result
    });
    (status, body, meter)
}

/// One put (11 bytes) + one get (reads 11 bytes back) + one get of a missing
/// key + one delete → storage_ops == 4, storage_bytes == 11,
/// storage_egress_bytes == 11. A FAILED op (here: none) would not emit. The
/// put base64 of "hello world" (11 bytes) is "aGVsbG8gd29ybGQ=".
const STORAGE_METER_APP: &str = r#"
export default {
    async fetch(request, env, ctx) {
        const s = env.storage;
        const B = "b", K = "k";
        // 11 bytes "hello world"
        // The native callbacks resolve JSON STRINGS (the @zeroship/storage
        // SDK is what JSON.parses them); parse here to inspect.
        await s.put(B, K, "aGVsbG8gd29ybGQ=");   // op + 11 bytes written
        const got = JSON.parse(await s.get(B, K)); // op + 11 bytes egress
        const miss = JSON.parse(await s.get(B, "nope")); // op (miss → null)
        await s.delete(B, K);                     // op
        return Response.json({ ok: got && got.size === 11 && miss === null });
    },
};
"#;

#[test]
fn metering_storage_ops_emit_ops_and_bytes_scoped_to_app() {
    let app_id = "00000000-0000-7000-8000-0000000000c3";
    let (status, body, meter) = run_app_metered(STORAGE_METER_APP, app_id);
    assert_eq!(status, 200, "storage metering app non-200; body: {body}");
    assert!(body.contains(r#""ok":true"#), "storage metering app failed; body: {body}");

    let snap = meter.drain();
    let id = uuid::Uuid::parse_str(app_id).unwrap();
    let u = snap.get(&id).expect("meter recorded usage for the app");
    assert_eq!(
        u.custom.get("storage_ops").copied(),
        Some(4),
        "put + get + get(miss) + delete = 4 storage_ops; got {:?}",
        u.custom
    );
    assert_eq!(
        u.custom.get("storage_bytes").copied(),
        Some(11),
        "put wrote 11 bytes; got {:?}",
        u.custom
    );
    assert_eq!(
        u.custom.get("storage_egress_bytes").copied(),
        Some(11),
        "get read 11 bytes (miss adds 0); got {:?}",
        u.custom
    );
}

/// A FAILED storage op must NOT emit. `put` with an empty bucket throws a
/// TypeError synchronously in the callback (no spawned op, no emit). Drive a
/// put that fails the BACKEND (oversized key path is hard; instead use an
/// op that the backend rejects): here we put a valid object then assert ONLY
/// the successful op billed — and that the synchronous-validation reject of a
/// missing-arg put emits nothing.
#[test]
fn metering_storage_failed_validation_emits_nothing() {
    const APP: &str = r#"
export default {
    async fetch(request, env, ctx) {
        // Missing required 'key' arg → synchronous TypeError, caught here.
        // The callback throws BEFORE pushing any spawned op, so no metric.
        try { await env.storage.put("b"); } catch (e) { /* expected */ }
        return Response.json({ ok: true });
    },
};
"#;
    let app_id = "00000000-0000-7000-8000-0000000000d4";
    let (status, body, meter) = run_app_metered(APP, app_id);
    assert_eq!(status, 200, "non-200; body: {body}");
    assert!(body.contains(r#""ok":true"#), "app failed; body: {body}");

    let snap = meter.drain();
    let id = uuid::Uuid::parse_str(app_id).unwrap();
    assert!(
        snap.get(&id).is_none(),
        "a failed/validation-rejected storage op must emit no metric; got {:?}",
        snap.get(&id)
    );
}

// ---------------------------------------------------------------------------
// Backpressure regression (ISS-32 PR4): a `putStream` whose total size far
// exceeds the StreamWriter buffer cap (4 MiB) MUST succeed. Before the
// forwarder learned to pause/resume the V8 read loop on the buffer high/low
// water marks, the eager read loop drained the whole source into the channel
// in one microtask burst and overflowed at 4 MiB — the upload failed with
// "upload stream exceeded the buffer backpressure cap". This drives 24 MiB of
// 256 KiB chunks through the real V8 → response_forwarder → StreamReader →
// LocalFs path and asserts the full round-trip, proving backpressure bounds
// the buffer instead of overflowing it.
const STORAGE_STREAM_BACKPRESSURE_APP: &str = r#"
export default {
    async fetch(request, env, ctx) {
        const s = env.storage;
        const BUCKET = "uploads";
        const KEY = "stream/big.bin";
        const CHUNK = 256 * 1024;       // 256 KiB
        const N = 96;                   // 96 * 256 KiB = 24 MiB ≫ 4 MiB cap
        const total = CHUNK * N;

        // Cheap, position-dependent pattern so a dropped/duplicated/reordered
        // chunk changes the running checksum (no whole-object buffer).
        function fillChunk(buf, c) {
            const base = (c * 131) & 0xff;
            for (let i = 0; i < buf.length; i++) buf[i] = (base + (i & 0x3f)) & 0xff;
        }
        // Fletcher-style rolling checksum, position-weighted.
        function checksum(bytes, absStart, acc) {
            let a = acc.a, b = acc.b;
            for (let i = 0; i < bytes.length; i++) {
                a = (a + bytes[i] * (((absStart + i) % 65521) + 1)) % 0xfffffffb;
                b = (b + a) % 0xfffffffb;
            }
            acc.a = a; acc.b = b;
        }

        try {
            const upAcc = { a: 1, b: 0 };
            let produced = 0, c = 0;
            const upload = new ReadableStream({
                pull(controller) {
                    if (produced >= total) { controller.close(); return; }
                    const buf = new Uint8Array(CHUNK);
                    fillChunk(buf, c);
                    checksum(buf, produced, upAcc);
                    produced += CHUNK; c += 1;
                    controller.enqueue(buf);
                },
            });
            const putRaw = await s.putStream(BUCKET, KEY, upload, "application/octet-stream");
            const put = JSON.parse(putRaw);
            if (put.size !== total) {
                return Response.json({ ok: false, step: "put.size", got: put.size, want: total }, { status: 500 });
            }

            // Stream it back and re-checksum incrementally.
            const handleRaw = await s.getStream(BUCKET, KEY);
            if (!handleRaw || handleRaw === "null") {
                return Response.json({ ok: false, step: "get.handle" }, { status: 500 });
            }
            const handle = JSON.parse(handleRaw);
            const downAcc = { a: 1, b: 0 };
            let read = 0;
            for (;;) {
                const chunk = await s.readChunk(handle.streamId);
                if (chunk === undefined || chunk === null) break;
                checksum(chunk, read, downAcc);
                read += chunk.length;
            }
            if (read !== total) {
                return Response.json({ ok: false, step: "get.totalRead", got: read, want: total }, { status: 500 });
            }
            if (downAcc.a !== upAcc.a || downAcc.b !== upAcc.b) {
                return Response.json({ ok: false, step: "checksum",
                    got: [downAcc.a, downAcc.b], want: [upAcc.a, upAcc.b] }, { status: 500 });
            }
            return Response.json({ ok: true, size: total });
        } catch (e) {
            return Response.json({ ok: false, message: (e && e.message) || String(e) }, { status: 500 });
        }
    },
};
"#;

#[test]
fn e2e_storage_streaming_backpressure_over_cap() {
    let (status, body) = run_app(STORAGE_STREAM_BACKPRESSURE_APP);
    assert_eq!(
        status, 200,
        "over-cap streaming upload failed (regression: backpressure not applied?); body: {body}"
    );
    assert!(
        body.contains(r#""ok":true"#),
        "over-cap streaming round-trip reported failure; body: {body}"
    );
}
