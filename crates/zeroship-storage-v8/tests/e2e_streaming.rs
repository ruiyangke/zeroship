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
//!       → Backend::get_stream → isolate-owned registry
//!         (cross-tenant isolation is covered by cross_tenant_streams.rs)
//!       → ResolveValue::Bytes
//!       → JS reassembles the object
//!
//! The handler self-asserts a multi-chunk upload → download round-trip
//! (byte-compare) and returns `{ok:true}`; the Rust side asserts 200+ok.
//!
//! Harness mirrors `crates/zeroship-kv-v8/tests/e2e_runtime.rs`.

#![allow(clippy::future_not_send)]

use std::collections::HashMap;
use std::sync::Arc;
use std::time::Duration;

use zeroship_runtime::channel::CancelFlag;
use zeroship_runtime::plugin::NativePlugin;
use zeroship_runtime::{
    init_v8, EnvSnapshot, FetchOutcome, ModuleEntry, RequestCtx, Runtime, SettledFetch,
};

use zeroship_storage::{LocalFs, StorageStore};
use zeroship_storage_v8::StorageBinding;

#[path = "../../../tests/fixtures/s3.rs"]
mod s3_fixture;

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
    let root = tempfile::tempdir().unwrap();
    let store = StorageStore::from_backend(Arc::new(LocalFs::new(root.path())));
    run_app_with_store(app, store)
}

fn run_app_with_store(app: &'static str, store: StorageStore) -> (u16, String) {
    compio::runtime::Runtime::new().unwrap().block_on(async move {
        init_v8();
        let env_vars = HashMap::from([("APP_ID".to_owned(), "e2e_app".to_owned())]);
        let plugin: Arc<dyn NativePlugin> = Arc::new(StorageBinding::new(store, None));

        let runtime = Runtime::builder()
            .modules(module(app))
            .env_vars(env_vars)
            .plugins(vec![plugin])
            .build();
        runtime.start_pump();

        let env = EnvSnapshot::empty();
        let ctx = RequestCtx::new(CancelFlag::new());
        let outcome = runtime.call_fetch_handler("GET", "http://localhost/", &[], "", &env, ctx);

        match outcome {
            FetchOutcome::Response { status, body, .. } => {
                (status, String::from_utf8_lossy(&body).into_owned())
            }
            FetchOutcome::Pending { rx, cancel: _ } => {
                let settled = compio::time::timeout(Duration::from_secs(30), rx.recv())
                    .await
                    .expect("storage e2e: fetch pending timed out")
                    .expect("storage e2e: pending delivered DispatchError");
                match settled {
                    SettledFetch::Response { status, body, .. } => {
                        (status, String::from_utf8_lossy(&body).into_owned())
                    }
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
        }
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
/// drives the REAL `StorageBinding::with_backend_and_meter` → register
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

        let backend: Arc<dyn zeroship_storage::Backend> = Arc::new(LocalFs::new(&dir));
        let plugin: Arc<dyn NativePlugin> =
            Arc::new(StorageBinding::new(StorageStore::from_backend(backend), Some(meter_for_run)));

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
            FetchOutcome::Response { status, body, .. } => {
                (status, String::from_utf8_lossy(&body).into_owned())
            }
            FetchOutcome::Pending { rx, cancel: _ } => {
                let settled = compio::time::timeout(Duration::from_secs(30), rx.recv())
                    .await
                    .expect("storage metering: fetch pending timed out")
                    .expect("storage metering: pending delivered DispatchError");
                match settled {
                    SettledFetch::Response { status, body, .. } => {
                        (status, String::from_utf8_lossy(&body).into_owned())
                    }
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

    let events = meter.drain();
    let id = zeroship_core::app_id::AppId::parse(app_id).unwrap();
    assert_eq!(
        usage_value(&events, &id, "storage_ops"),
        Some(4),
        "put + get + get(miss) + delete = 4 storage_ops; got {events:?}"
    );
    assert_eq!(
        usage_value(&events, &id, "storage_bytes"),
        Some(11),
        "put wrote 11 bytes; got {events:?}"
    );
    assert_eq!(
        usage_value(&events, &id, "storage_egress_bytes"),
        Some(11),
        "get read 11 bytes (miss adds 0); got {events:?}"
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

    let events = meter.drain();
    let id = zeroship_core::app_id::AppId::parse(app_id).unwrap();
    assert!(
        !events.iter().any(|event| event.subject.app.as_ref() == Some(&id)),
        "a failed/validation-rejected storage op must emit no metric; got {:?}",
        events
    );
}

fn usage_value(
    events: &[zeroship_core::usage_event::UsageEvent],
    app_id: &zeroship_core::app_id::AppId,
    meter: &str,
) -> Option<u64> {
    events
        .iter()
        .find(|event| event.subject.app.as_ref() == Some(app_id) && event.meter == meter)
        .map(|event| event.value)
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

// A producer that yields several good chunks and then THROWS. The stream is
// errored, not closed: no well-formed object was ever produced, so a commit
// here stores a prefix of the bytes the app tried to upload.
//
// The handler reports which of the two happened rather than asserting, so a
// failure names the observed size instead of just a rejected promise.
const STORAGE_STREAM_PRODUCER_THROWS_APP: &str = r#"
export default {
    async fetch(request, env, ctx) {
        const s = env.storage;
        const BUCKET = "uploads";
        const KEY = "stream/truncated.bin";
        const CHUNK = 64 * 1024;
        const GOOD = 4;

        let produced = 0;
        const upload = new ReadableStream({
            pull(controller) {
                if (produced < GOOD) {
                    controller.enqueue(new Uint8Array(CHUNK));
                    produced += 1;
                    return;
                }
                throw new Error("producer exploded mid-upload");
            },
        });

        let putResult = null;
        try {
            putResult = JSON.parse(await s.putStream(BUCKET, KEY, upload, "application/octet-stream"));
        } catch (e) {
            // Desired: the failed producer surfaces as a failed upload.
            return Response.json({ ok: true, rejected: (e && e.message) || String(e) });
        }

        // putStream RESOLVED despite the producer failing. Report what it
        // committed, and whether the truncated object is now readable back.
        let storedSize = null;
        try {
            const handleRaw = await s.getStream(BUCKET, KEY);
            if (handleRaw && handleRaw !== "null") {
                const handle = JSON.parse(handleRaw);
                storedSize = 0;
                for (;;) {
                    const chunk = await s.readChunk(handle.streamId);
                    if (chunk === undefined || chunk === null) break;
                    storedSize += chunk.length;
                }
            }
        } catch (e) {
            storedSize = "readback threw: " + ((e && e.message) || String(e));
        }
        return Response.json({
            ok: false,
            step: "put.resolved",
            committedSize: putResult && putResult.size,
            producedBeforeThrow: produced * CHUNK,
            storedSize,
        }, { status: 500 });
    },
};
"#;

/// A creator upload whose `ReadableStream` throws must not be committed as a
/// complete object. The producer's failure reaches the forwarder as a rejected
/// `read()`; if that is forwarded as a plain EOF, the consumer finishes the
/// multipart and `putStream` resolves with the truncated size, which is also
/// what metering then bills.
#[test]
fn e2e_storage_upload_with_a_throwing_producer_is_not_committed() {
    let (status, body) = run_app(STORAGE_STREAM_PRODUCER_THROWS_APP);
    assert_eq!(
        status, 200,
        "a throwing upload producer was committed as a successful object; body: {body}"
    );
    assert!(
        body.contains(r#""ok":true"#),
        "putStream did not reject on producer failure; body: {body}"
    );
    // WITNESS that the scenario actually occurred. Without this the test passes
    // on ANY rejection - a bad bucket name, a validation refusal, a backend
    // that never started - none of which exercise the producer-failure path.
    // The rejection must carry the producer's own message, which only the
    // abort-note path can put there.
    assert!(
        body.contains("producer exploded mid-upload"),
        "putStream rejected, but not for the producer's failure, so this test \
         did not exercise the path it names; body: {body}"
    );
}

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

#[test]
fn e2e_storage_streaming_s3() {
    let minio = s3_fixture::Minio::start();
    let backend = zeroship_storage::S3::with_tuning(
        minio.config("v8"),
        minio.credentials(),
        zeroship_storage::S3UploadTuning::DEFAULTS,
    );
    let store = StorageStore::from_backend(Arc::new(backend));
    let (status, body) = run_app_with_store(STORAGE_STREAM_E2E_APP, store);
    assert_eq!(status, 200, "storage S3 binding failed: {body}");
    assert_eq!(serde_json::from_str::<serde_json::Value>(&body).unwrap()["ok"], true);
}
