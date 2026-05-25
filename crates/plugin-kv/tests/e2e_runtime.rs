//! End-to-end integration test for `env.kv` — drives JS through the REAL
//! V8 runtime + compio event loop down to a live backend, identically for
//! both backends (redb embedded, Dragonfly/Redis network).
//!
//! ## What this covers that nothing else does
//!
//! Existing tests exercise the backends in isolation and the `@zeroship/kv`
//! SDK against a mock. NOTHING exercises the real path:
//!
//!     JS `await env.kv.x()`
//!       → V8 arg marshalling
//!       → `dispatch_*`
//!       → spawned op
//!       → event-loop pump
//!       → backend
//!       → resolve
//!       → JS sees the value
//!
//! This test closes that gap. The fetch handler below calls `env.kv.*`
//! directly (the native surface, NOT the SDK) and self-asserts every step.
//! On any mismatch it returns `{ok:false, step, got, want}` with status 500;
//! the Rust side asserts status 200 + `ok:true`, and the body names the
//! failing step on a mismatch.
//!
//! ## Harness
//!
//! Mirrors `crates/runtime/tests/call_fetch_handler.rs::async_response`:
//! build a `Runtime` with the JS app + `KvPlugin::with_backend(...)` + an
//! `APP_ID` env var, `start_pump()`, `call_fetch_handler(...)`, then drive
//! the (likely Pending) outcome to a `SettledFetch::Response` via the
//! receiver. plugin-kv's test crate can't import runtime's test-only
//! `common` module, so the minimal pieces are replicated inline.

use std::collections::HashMap;
use std::sync::Arc;
use std::time::Duration;

use zeroship_runtime::channel::CancelFlag;
use zeroship_runtime::plugin::NativePlugin;
use zeroship_runtime::{
    init_v8, EnvSnapshot, FetchOutcome, ModuleEntry, RequestCtx, Runtime, SettledFetch,
};

use zeroship_plugin_kv::{Backend, KvPlugin};

#[cfg(feature = "redb")]
use zeroship_plugin_kv::RedbBackend;
#[cfg(feature = "redis")]
use zeroship_plugin_kv::Redis;

// ---------------------------------------------------------------------------
// The JS app — exercises the full `env.kv` surface and self-asserts.
// ---------------------------------------------------------------------------
//
// A single `default.fetch` handler runs a sequence of assertions. Each
// assertion records a step into a trace; on the first mismatch the handler
// returns Response.json({ok:false, step, got, want}, {status:500}). If every
// step passes it returns Response.json({ok:true, trace}).
//
// A unique per-run key prefix (random suffix) keeps reruns against the
// persistent backends from colliding.
const KV_E2E_APP: &str = r#"
export default {
    async fetch(request, env, ctx) {
        const kv = env.kv;
        // Unique prefix per run so reruns against persistent backends
        // (redb file / live Dragonfly) never collide.
        const P = "e2e:" + Math.random().toString(36).slice(2) + ":";
        const k = (s) => P + s;
        const trace = [];

        // fail(step, got, want) → throws a structured marker the catch
        // below turns into the {ok:false,...} body.
        function fail(step, got, want) {
            const e = new Error("step failed: " + step);
            e.zsStep = step;
            e.zsGot = got;
            e.zsWant = want;
            throw e;
        }
        function eq(step, got, want) {
            trace.push(step);
            if (got !== want) fail(step, got, want);
        }
        function truthy(step, got) {
            trace.push(step);
            if (!got) fail(step, got, "truthy");
        }

        try {
            // 1. set/get round-trip
            {
                const r = await kv.set(k("k1"), "v1");
                truthy("1.set.ok", r && r.ok === true);
                eq("1.get.k1", await kv.get(k("k1")), "v1");
                eq("1.get.missing", await kv.get(k("missing")), null);
            }

            // 2. TTL
            {
                await kv.set(k("k2"), "v", { ttlMs: 60000 });
                const t = await kv.ttl(k("k2"));
                truthy("2.ttl.k2.obj", t && typeof t === "object");
                truthy("2.ttl.k2.range",
                    typeof t.ttlMs === "number" && t.ttlMs > 0 && t.ttlMs <= 60000);
                await kv.set(k("k3"), "v");
                const t3 = await kv.ttl(k("k3"));
                truthy("2.ttl.k3.obj", t3 && typeof t3 === "object");
                eq("2.ttl.k3.noexpiry", t3.ttlMs, null);
                eq("2.ttl.missing", await kv.ttl(k("missing")), null);
            }

            // 3. incr
            {
                eq("3.incr.c1.1", await kv.incr(k("c1")), 1);
                eq("3.incr.c1.by5", await kv.incr(k("c1"), { by: 5 }), 6);
                eq("3.incr.c1.byneg2", await kv.incr(k("c1"), { by: -2 }), 4);
            }

            // 4. incr TTL-on-create, preserved across subsequent incr
            {
                eq("4.incr.c2.create", await kv.incr(k("c2"), { ttlMs: 60000 }), 1);
                const t1 = await kv.ttl(k("c2"));
                truthy("4.ttl.c2.set", t1 && typeof t1.ttlMs === "number" && t1.ttlMs > 0);
                await kv.incr(k("c2"));
                const t2 = await kv.ttl(k("c2"));
                truthy("4.ttl.c2.preserved",
                    t2 && typeof t2.ttlMs === "number" && t2.ttlMs > 0);
            }

            // 5. incr errors: non-numeric + overflow
            {
                await kv.set(k("s1"), "abc");
                let code = null;
                try { await kv.incr(k("s1")); }
                catch (e) { code = e && e.code; }
                eq("5.incr.nonnumeric.code", code, "kv_non_numeric");

                await kv.set(k("s2"), String(2n ** 63n - 1n)); // i64::MAX
                let code2 = null;
                try { await kv.incr(k("s2")); }
                catch (e) { code2 = e && e.code; }
                eq("5.incr.overflow.code", code2, "kv_overflow");
            }

            // 6. bigint — native resolves BigInt above 2^53
            {
                await kv.set(k("big"), String(2n ** 53n));
                const r = await kv.incr(k("big"), { by: 10 });
                eq("6.incr.big.type", typeof r, "bigint");
                eq("6.incr.big.value", r, 2n ** 53n + 10n);
            }

            // 7. setIfAbsent
            {
                const a = await kv.setIfAbsent(k("lock"), "a");
                eq("7.sia.first.stored", a && a.stored, true);
                const b = await kv.setIfAbsent(k("lock"), "b");
                eq("7.sia.second.stored", b && b.stored, false);
                eq("7.sia.get", await kv.get(k("lock")), "a");
            }

            // 8. expire / persist
            {
                await kv.set(k("e1"), "v");
                const ex = await kv.expire(k("e1"), 60000);
                eq("8.expire.updated", ex && ex.updated, true);
                const t = await kv.ttl(k("e1"));
                truthy("8.expire.ttl", t && typeof t.ttlMs === "number" && t.ttlMs > 0);
                const pe = await kv.persist(k("e1"));
                eq("8.persist.updated", pe && pe.updated, true);
                const t2 = await kv.ttl(k("e1"));
                truthy("8.persist.ttl.obj", t2 && typeof t2 === "object");
                eq("8.persist.ttl.null", t2.ttlMs, null);
                const nope = await kv.expire(k("nope"), 1000);
                eq("8.expire.missing", nope && nope.updated, false);
            }

            // 9. delete
            {
                const d = await kv.delete(k("k1"));
                eq("9.delete.first", d && d.deleted, true);
                eq("9.delete.get", await kv.get(k("k1")), null);
                const d2 = await kv.delete(k("k1"));
                eq("9.delete.second", d2 && d2.deleted, false);
            }

            // 10. list pagination — set 5 keys, follow cursor until null
            {
                for (let i = 0; i < 5; i++) {
                    await kv.set(k("page:" + i), "x");
                }
                const collected = new Set();
                let cursor = undefined;
                let iters = 0;
                let terminated = false;
                while (iters < 10) {
                    iters++;
                    const opts = { limit: 2 };
                    if (cursor != null) opts.cursor = cursor;
                    const res = await kv.list(k("page:"), opts);
                    truthy("10.list.res", res && Array.isArray(res.keys));
                    for (const key of res.keys) collected.add(key);
                    cursor = res.cursor;
                    if (cursor == null) { terminated = true; break; }
                }
                truthy("10.list.terminated", terminated);
                eq("10.list.count", collected.size, 5);
                for (let i = 0; i < 5; i++) {
                    truthy("10.list.has." + i, collected.has(k("page:" + i)));
                }
            }

            // 11. synchronous validation throws
            {
                let threwEmpty = false;
                try { await kv.get(""); }
                catch (e) { threwEmpty = true; }
                truthy("11.empty.throws", threwEmpty);

                let threwBrace = false;
                try { await kv.get(k("a{b")); }
                catch (e) { threwBrace = true; }
                truthy("11.brace.throws", threwBrace);
            }

            return Response.json({ ok: true, trace });
        } catch (e) {
            if (e && e.zsStep) {
                return Response.json({
                    ok: false,
                    step: e.zsStep,
                    got: e.zsGot === undefined ? null : e.zsGot,
                    want: e.zsWant === undefined ? null : e.zsWant,
                    trace,
                }, { status: 500 });
            }
            // Unexpected throw (e.g. a step that should have succeeded
            // rejected) — surface message + last trace entry.
            return Response.json({
                ok: false,
                step: trace.length ? trace[trace.length - 1] : "<none>",
                error: String(e && e.message ? e.message : e),
                code: e && e.code ? e.code : null,
                trace,
            }, { status: 500 });
        }
    }
};
"#;

/// Single module-entry list from a JS source string (replicates
/// runtime test-common's `m()`).
fn module(source: &str) -> Vec<ModuleEntry> {
    vec![ModuleEntry {
        specifier: "index.js".into(),
        source: source.into(),
    }]
}

/// Build a Runtime around the KV E2E app + the supplied backend, pump it,
/// call the fetch handler, and return `(status, body)`. The handler is
/// async (every assertion awaits a KV op), so `call_fetch_handler` returns
/// `Pending` and the pump delivers the final `SettledFetch` via the
/// receiver — same idiom as `call_fetch_handler.rs::async_response`.
fn run_e2e(backend: Arc<dyn Backend>) -> (u16, String) {
    compio::runtime::Runtime::new().unwrap().block_on(async move {
        init_v8();

        // APP_ID flows through env_vars → build_instance reads it to scope
        // the per-app key namespace (see crates/runtime/src/core/plugin.rs).
        let mut env_vars = HashMap::new();
        env_vars.insert("APP_ID".to_string(), "e2e_app".to_string());

        let plugin: Arc<dyn NativePlugin> = Arc::new(KvPlugin::with_backend(backend));

        let runtime = Runtime::builder()
            .modules(module(KV_E2E_APP))
            .env_vars(env_vars)
            .plugins(vec![plugin])
            .build();
        runtime.start_pump();

        let env = EnvSnapshot::empty();
        let ctx = RequestCtx::new(CancelFlag::new());
        let outcome =
            runtime.call_fetch_handler("GET", "http://localhost/", &[], "", &env, ctx);

        match outcome {
            FetchOutcome::Response { status, body, .. } => (status, body),
            FetchOutcome::Pending { rx, cancel: _ } => {
                let settled = compio::time::timeout(Duration::from_secs(30), rx.recv())
                    .await
                    .expect("kv e2e: fetch pending timed out")
                    .expect("kv e2e: pending delivered DispatchError");
                match settled {
                    SettledFetch::Response { status, body, .. } => (status, body),
                    other => {
                        let name = match other {
                            SettledFetch::Stream { .. } => "Stream",
                            SettledFetch::WebSocketUpgrade { .. } => "WebSocketUpgrade",
                            SettledFetch::Response { .. } => unreachable!(),
                        };
                        panic!("kv e2e: expected SettledFetch::Response, got {name}");
                    }
                }
            }
            FetchOutcome::Stream { .. } => panic!("kv e2e: unexpected Stream outcome"),
            FetchOutcome::WebSocketUpgrade { .. } => {
                panic!("kv e2e: unexpected WebSocketUpgrade outcome")
            }
        }
    })
}

/// Assert a successful end-to-end run: status 200 and `ok:true`. On
/// failure the body names the failing step (and got/want), so surface it
/// verbatim in the panic message.
fn assert_ok(status: u16, body: &str) {
    assert_eq!(status, 200, "kv e2e handler returned non-200; body: {body}");
    assert!(
        body.contains(r#""ok":true"#),
        "kv e2e handler reported failure; body: {body}"
    );
}

// ---------------------------------------------------------------------------
// redb (embedded) — always runs.
// ---------------------------------------------------------------------------

#[cfg(feature = "redb")]
#[test]
fn e2e_redb() {
    let dir = tempfile::tempdir().expect("create tempdir");
    let path = dir.path().join("kv.redb");
    let backend = RedbBackend::open(&path).expect("open redb backend");
    let (status, body) = run_e2e(Arc::new(backend));
    assert_ok(status, &body);
}

// ---------------------------------------------------------------------------
// Dragonfly / Redis (live single-node) — runs only when ZEROSHIP_KV_URL is
// set (e.g. redis://127.0.0.1:6399). Skips (does not fail) when unset so CI
// without a server is green.
// ---------------------------------------------------------------------------

#[cfg(feature = "redis")]
#[test]
fn e2e_dragonfly() {
    let Ok(url) = std::env::var("ZEROSHIP_KV_URL") else {
        eprintln!(
            "e2e_dragonfly: ZEROSHIP_KV_URL unset — skipping live-backend test. \
             Set e.g. ZEROSHIP_KV_URL=redis://127.0.0.1:6399 to run it."
        );
        return;
    };
    let backend = Redis::new(url);
    let (status, body) = run_e2e(Arc::new(backend));
    assert_ok(status, &body);
}

// ---------------------------------------------------------------------------
// Dragonfly CLUSTER (live 3-node) — runs only when DRAGONFLY_CLUSTER_SEEDS is
// set (comma-joined seed URLs, e.g.
// redis://127.0.0.1:7000,redis://127.0.0.1:7001,redis://127.0.0.1:7002).
// Skips (does not fail) when unset.
//
// This is the ONLY end-to-end coverage of the cluster code path through the
// real runtime: hash-tag scoping keeps an app's keys in one slot, the `incr`
// Lua EVAL routes to the right node, SCAN-based `list` routes correctly, and
// SET NX / pexpire / pttl / persist all work through the cluster client. The
// JS app + assertions are reused verbatim from `run_e2e`.
// ---------------------------------------------------------------------------

#[cfg(feature = "redis")]
#[test]
fn e2e_dragonfly_cluster() {
    let Ok(seeds) = std::env::var("DRAGONFLY_CLUSTER_SEEDS") else {
        eprintln!(
            "e2e_dragonfly_cluster: DRAGONFLY_CLUSTER_SEEDS unset — skipping live-cluster \
             test. Set e.g. DRAGONFLY_CLUSTER_SEEDS=\
             redis://127.0.0.1:7000,redis://127.0.0.1:7001,redis://127.0.0.1:7002 to run it."
        );
        return;
    };
    // Build the plugin-kv cluster URL exactly like
    // redis_backend.rs::cluster_url(): base URL = first seed, plus
    // ?cluster=true&seeds=<comma-joined seeds>.
    let first = seeds.split(',').next().map(str::trim).unwrap_or("").to_string();
    if first.is_empty() {
        eprintln!("e2e_dragonfly_cluster: DRAGONFLY_CLUSTER_SEEDS empty — skipping.");
        return;
    }
    let cluster_url = format!("{first}?cluster=true&seeds={seeds}");
    let backend = Redis::new(cluster_url);
    let (status, body) = run_e2e(Arc::new(backend));
    assert_ok(status, &body);
}
