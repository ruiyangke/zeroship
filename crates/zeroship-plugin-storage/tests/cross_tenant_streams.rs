//! Cross-tenant isolation for the `env.storage` download-stream registry.
//!
//! The registry backing `getStream` / `readChunk` / `cancelStream` is a
//! `thread_local!`, and the worker runs MANY apps' isolates on ONE OS thread
//! (`crates/worker/src/cache.rs`; the invariant is stated in AGENTS.md under
//! "V8 per thread"). So the registry — and the monotonic id counter that
//! makes its ids enumerable from 1 — is shared by every app on that thread.
//!
//! These tests reproduce the real shape: TWO `Runtime`s carrying two
//! different `APP_ID`s, built and driven on ONE thread inside a single compio
//! runtime, over a SHARED `LocalFs` root. Nothing is stubbed — the JS calls
//! the registered native callbacks through V8 exactly as a deployed app does.
//!
//! What is asserted:
//!
//! 1. `readChunk` under app B cannot reach a stream opened by app A
//!    (`cross_tenant_read_chunk_cannot_reach_another_apps_stream`).
//! 2. `cancelStream` under app B cannot destroy a stream opened by app A
//!    (same test — A drains successfully AFTER B has cancelled every id).
//! 3. Undrained streams are bounded per app, so one app cannot pin
//!    unbounded fds / HTTP bodies
//!    (`undrained_get_streams_are_capped_per_app`), and the cap is charged
//!    per app rather than per thread
//!    (`the_live_stream_cap_is_per_app_not_per_thread`).

#![allow(clippy::future_not_send)]

use std::collections::HashMap;
use std::path::Path;
use std::sync::Arc;
use std::time::Duration;

use zeroship_runtime::channel::CancelFlag;
use zeroship_runtime::plugin::NativePlugin;
use zeroship_runtime::{
    init_v8, EnvSnapshot, FetchOutcome, ModuleEntry, RequestCtx, Runtime, SettledFetch,
};

use zeroship_plugin_storage::StoragePlugin;

// ---------------------------------------------------------------------------
// Harness — two isolates, one thread, one shared LocalFs root
// ---------------------------------------------------------------------------

fn module(source: &str) -> Vec<ModuleEntry> {
    vec![ModuleEntry { specifier: "index.js".into(), source: source.into() }]
}

/// Build a real `Runtime` for `app_id` over `dir`, with the REAL
/// `StoragePlugin` registered (same path the worker takes).
///
/// The trailing `exit_isolate()` mirrors `crates/worker/src/cache.rs:405`
/// ("Exit isolate so other isolates can be created/entered on this thread") —
/// it is what lets several isolates coexist on one OS thread, and therefore
/// what makes a `thread_local!` registry shared across apps.
fn build_runtime(app_id: &str, dir: &Path, source: &str) -> Runtime {
    let mut env_vars = HashMap::new();
    env_vars.insert("APP_ID".to_string(), app_id.to_string());

    let plugin: Arc<dyn NativePlugin> = Arc::new(StoragePlugin::local(dir));

    let runtime = Runtime::builder()
        .modules(module(source))
        .env_vars(env_vars)
        .plugins(vec![plugin])
        .build();
    runtime.start_pump();
    runtime.exit_isolate();
    runtime
}

/// Drive one fetch through `runtime` and settle it. `headers` carries the
/// cross-tenant stream id so app B can name an id it was never given.
///
/// `enter_isolate` / `exit_isolate` around the dispatch is mandatory for
/// multi-isolate-per-thread callers (`Runtime::enter_isolate` doc,
/// `crates/runtime/src/core/runtime.rs:492`) and is exactly what
/// `crates/worker/src/handler.rs:386-396` does per request.
async fn fetch(runtime: &Runtime, headers: &[(String, String)]) -> (u16, String) {
    let env = EnvSnapshot::empty();
    let ctx = RequestCtx::new(CancelFlag::new());
    runtime.enter_isolate();
    let outcome =
        runtime.call_fetch_handler("GET", "http://localhost/", headers, "", &env, ctx);
    runtime.exit_isolate();

    match outcome {
        FetchOutcome::Response { status, body, .. } => {
            (status, String::from_utf8_lossy(&body).into_owned())
        }
        FetchOutcome::Pending { rx, cancel: _ } => {
            let settled = compio::time::timeout(Duration::from_secs(30), rx.recv())
                .await
                .expect("cross-tenant: fetch pending timed out")
                .expect("cross-tenant: pending delivered DispatchError");
            match settled {
                SettledFetch::Response { status, body, .. } => {
                    (status, String::from_utf8_lossy(&body).into_owned())
                }
                _ => panic!("cross-tenant: expected SettledFetch::Response"),
            }
        }
        _ => panic!("cross-tenant: unexpected FetchOutcome"),
    }
}

fn scratch_dir(tag: &str) -> std::path::PathBuf {
    static SEQ: std::sync::atomic::AtomicU32 = std::sync::atomic::AtomicU32::new(0);
    let seq = SEQ.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
    let dir =
        std::env::temp_dir().join(format!("zs-storage-xtenant-{tag}-{}-{seq}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    dir
}

fn json_field<'a>(body: &'a str, key: &str) -> Option<&'a str> {
    let needle = format!("\"{key}\":");
    let start = body.find(&needle)? + needle.len();
    let rest = &body[start..];
    let end = rest.find([',', '}'])?;
    Some(rest[..end].trim().trim_matches('"'))
}

// ---------------------------------------------------------------------------
// Apps
// ---------------------------------------------------------------------------

/// App A, phase 1: write an object and OPEN a download stream, then return
/// its id WITHOUT draining it. The stream stays live in the registry.
const APP_A_OPEN: &str = r#"
export default {
    async fetch(request, env, ctx) {
        const s = env.storage;
        // 12 bytes: "APPLE-SECRET" — recognisable in a leak.
        await s.put("uploads", "secret.bin", "QVBQTEUtU0VDUkVU");
        const handle = JSON.parse(await s.getStream("uploads", "secret.bin"));
        return Response.json({ ok: true, streamId: handle.streamId, size: handle.size });
    },
};
"#;

/// App A, phase 3: drain the stream opened in phase 1 and report the bytes.
/// If app B's `cancelStream` reached across the tenancy boundary, this reads
/// zero bytes instead of the object.
const APP_A_DRAIN: &str = r#"
export default {
    async fetch(request, env, ctx) {
        const s = env.storage;
        const id = Number(request.headers.get("x-stream-id"));
        let text = "";
        let n = 0;
        for (;;) {
            const chunk = await s.readChunk(id);
            if (chunk === undefined || chunk === null) break;
            n += chunk.length;
            text += new TextDecoder().decode(chunk);
        }
        return Response.json({ ok: true, read: n, text });
    },
};
"#;

/// App B: try to reach app A's stream. Reads the id A was actually handed
/// (via header) AND sweeps the low ids, because the counter is monotonic
/// from 1 and therefore enumerable by construction. Then cancels every one
/// of them. A correct registry gives B nothing and destroys nothing.
const APP_B_PROBE: &str = r#"
export default {
    async fetch(request, env, ctx) {
        const s = env.storage;
        const victim = Number(request.headers.get("x-stream-id"));
        const ids = [victim];
        for (let i = 1; i <= 8; i++) if (!ids.includes(i)) ids.push(i);

        let leakedBytes = 0;
        let leakedText = "";
        for (const id of ids) {
            const chunk = await s.readChunk(id);
            if (chunk !== undefined && chunk !== null) {
                leakedBytes += chunk.length;
                leakedText += new TextDecoder().decode(chunk);
            }
        }
        // Now try to destroy every one of them.
        for (const id of ids) await s.cancelStream(id);

        return Response.json({ ok: true, leakedBytes, leakedText, probed: ids.length });
    },
};
"#;

// ---------------------------------------------------------------------------
// 1 + 2: read and cancel must not cross the tenancy boundary
// ---------------------------------------------------------------------------

#[test]
fn cross_tenant_read_chunk_cannot_reach_another_apps_stream() {
    compio::runtime::Runtime::new().unwrap().block_on(async move {
        init_v8();
        let dir = scratch_dir("read");

        // Two apps, two isolates, ONE thread, one shared storage root —
        // the worker's documented steady state.
        let rt_a_open = build_runtime("app_alpha", &dir, APP_A_OPEN);
        let rt_b = build_runtime("app_beta", &dir, APP_B_PROBE);
        let rt_a_drain = build_runtime("app_alpha", &dir, APP_A_DRAIN);

        // -- phase 1: A opens a stream and leaves it live -------------------
        let (status, body) = fetch(&rt_a_open, &[]).await;
        assert_eq!(status, 200, "app A open failed; body: {body}");
        let stream_id = json_field(&body, "streamId")
            .unwrap_or_else(|| panic!("no streamId in A's response: {body}"))
            .to_string();
        assert_eq!(
            json_field(&body, "size"),
            Some("12"),
            "A's object should be 12 bytes; body: {body}"
        );

        let hdr = vec![("x-stream-id".to_string(), stream_id.clone())];

        // -- phase 2: B probes and cancels every id it can name -------------
        let (status, body_b) = fetch(&rt_b, &hdr).await;
        assert_eq!(status, 200, "app B probe failed; body: {body_b}");

        assert_eq!(
            json_field(&body_b, "leakedBytes"),
            Some("0"),
            "CROSS-TENANT READ: app B read another app's object bytes through \
             the shared thread-local stream registry (A's id was {stream_id}); \
             B's response: {body_b}"
        );
        assert!(
            !body_b.contains("APPLE-SECRET"),
            "CROSS-TENANT READ: app B received app A's object CONTENT; \
             B's response: {body_b}"
        );

        // -- phase 3: A's stream must have survived B's cancel sweep --------
        let (status, body_a) = fetch(&rt_a_drain, &hdr).await;
        assert_eq!(status, 200, "app A drain failed; body: {body_a}");
        assert_eq!(
            json_field(&body_a, "read"),
            Some("12"),
            "CROSS-TENANT CANCEL: app B's cancelStream destroyed app A's \
             in-flight download (A expected to drain 12 bytes from id \
             {stream_id}); A's response: {body_a}"
        );
        assert!(
            body_a.contains("APPLE-SECRET"),
            "app A did not read back its own object; A's response: {body_a}"
        );

        let _ = std::fs::remove_dir_all(&dir);
    });
}


// ---------------------------------------------------------------------------
// 3: undrained streams are bounded, and bounded PER APP
// ---------------------------------------------------------------------------

/// Open `n` download streams and never drain them. Each live entry pins an
/// fd (LocalFs) or a live HTTP body (S3) — process-wide resources — so an
/// app must not be able to accumulate them without limit.
const APP_LEAK: &str = r#"
export default {
    async fetch(request, env, ctx) {
        const s = env.storage;
        const n = Number(request.headers.get("x-open-count"));
        await s.put("uploads", "leak.bin", "QVBQTEUtU0VDUkVU");
        let opened = 0;
        let error = null;
        for (let i = 0; i < n; i++) {
            try {
                const raw = await s.getStream("uploads", "leak.bin");
                if (!raw || raw === "null") { error = "unexpected null handle"; break; }
                opened += 1;
            } catch (e) {
                error = (e && e.message) || String(e);
                break;
            }
        }
        return Response.json({ ok: true, opened, error });
    },
};
"#;

#[test]
fn undrained_get_streams_are_capped_per_app() {
    compio::runtime::Runtime::new().unwrap().block_on(async move {
        init_v8();
        let dir = scratch_dir("leak");
        let cap = zeroship_plugin_storage::limits::max_live_get_streams_per_app();

        let rt = build_runtime("app_leaky", &dir, APP_LEAK);
        // Ask for well past the cap in ONE request; every handle is left
        // undrained, so nothing reclaims between iterations.
        let want = cap + 25;
        let hdr = vec![("x-open-count".to_string(), want.to_string())];
        let (status, body) = fetch(&rt, &hdr).await;
        assert_eq!(status, 200, "leak app failed; body: {body}");

        let opened: usize = json_field(&body, "opened")
            .unwrap_or_else(|| panic!("no opened count: {body}"))
            .parse()
            .unwrap_or_else(|_| panic!("opened not a number: {body}"));

        assert!(
            opened <= cap,
            "UNBOUNDED LEAK: an app opened {opened} undrained download streams \
             with a cap of {cap}; each pins an fd / live HTTP body. body: {body}"
        );
        assert_eq!(
            opened, cap,
            "the cap should admit exactly {cap} live streams before refusing; \
             body: {body}"
        );
        assert!(
            body.contains("live download streams"),
            "the refusal must name the cap so a creator can act on it; body: {body}"
        );

        let _ = std::fs::remove_dir_all(&dir);
    });
}

#[test]
fn the_live_stream_cap_is_per_app_not_per_thread() {
    compio::runtime::Runtime::new().unwrap().block_on(async move {
        init_v8();
        let dir = scratch_dir("percap");
        let cap = zeroship_plugin_storage::limits::max_live_get_streams_per_app();

        // App one exhausts its own budget on this thread...
        let rt_one = build_runtime("app_one", &dir, APP_LEAK);
        let hdr_full = vec![("x-open-count".to_string(), (cap + 5).to_string())];
        let (status, body_one) = fetch(&rt_one, &hdr_full).await;
        assert_eq!(status, 200, "app_one failed; body: {body_one}");
        assert_eq!(
            json_field(&body_one, "opened"),
            Some(cap.to_string().as_str()),
            "app_one should have filled its budget; body: {body_one}"
        );

        // ...and a second app on the SAME thread must be unaffected. A
        // per-thread cap would starve it (denial of service across tenants).
        let rt_two = build_runtime("app_two", &dir, APP_LEAK);
        let hdr_few = vec![("x-open-count".to_string(), "3".to_string())];
        let (status, body_two) = fetch(&rt_two, &hdr_few).await;
        assert_eq!(status, 200, "app_two failed; body: {body_two}");
        assert_eq!(
            json_field(&body_two, "opened"),
            Some("3"),
            "CROSS-TENANT DoS: app_one exhausting its stream budget blocked \
             app_two on the same thread; body: {body_two}"
        );

        let _ = std::fs::remove_dir_all(&dir);
    });
}
