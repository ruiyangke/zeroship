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

// ---------------------------------------------------------------------------
// Validation / type-error app — drives MALFORMED inputs through `env.kv.*`
// and pins each synchronous throw's message / `.code`.
// ---------------------------------------------------------------------------
//
// These throws fire in the v8_class method body (key shape, value type +
// size, option ranges) BEFORE any async op is dispatched, so they're
// backend-agnostic — redb is enough to exercise them. The handler runs a
// table of cases; each calls a malformed op inside try/catch, captures the
// thrown error, and asserts (a) it threw and (b) the message substring or
// `.code` matches. On the first mismatch it returns
// {ok:false, step, detail} at 500; if all pass it returns {ok:true}.
const KV_VALIDATION_APP: &str = r#"
export default {
    async fetch(request, env, ctx) {
        const kv = env.kv;
        const trace = [];

        // Run `fn` (which awaits a malformed kv op), assert it threw, and
        // assert the thrown error's `.message` contains `wantMsg` (when
        // given) and `.code === wantCode` (when given). Records the step.
        async function expectThrow(step, fn, { wantMsg, wantCode } = {}) {
            trace.push(step);
            let threw = false;
            let err = null;
            try { await fn(); }
            catch (e) { threw = true; err = e; }
            if (!threw) {
                return { step, detail: "did not throw" };
            }
            const msg = err && err.message ? String(err.message) : "";
            if (wantMsg != null && !msg.includes(wantMsg)) {
                return { step, detail: "message mismatch: got=" + msg + " want~=" + wantMsg };
            }
            if (wantCode != null && (err && err.code) !== wantCode) {
                return {
                    step,
                    detail: "code mismatch: got=" + (err && err.code) + " want=" + wantCode,
                };
            }
            return null;
        }

        try {
            const cases = [
                // --- value type errors (extract_value + js_type_name arms) ---
                ["set.value.number", () => kv.set("k", 123),
                    { wantMsg: "value must be a string" }],
                ["set.value.object", () => kv.set("k", { a: 1 }),
                    { wantMsg: "value must be a string" }],
                ["set.value.array", () => kv.set("k", [1, 2]),
                    { wantMsg: "value must be a string" }],
                ["set.value.boolean", () => kv.set("k", true),
                    { wantMsg: "value must be a string" }],
                // js_type_name boolean/array arms specifically surface in the
                // message tail "got <type>".
                ["set.value.boolean.typename", () => kv.set("k", true),
                    { wantMsg: "got boolean" }],
                ["set.value.array.typename", () => kv.set("k", [1, 2]),
                    { wantMsg: "got array" }],
                ["set.value.number.typename", () => kv.set("k", 123),
                    { wantMsg: "got number" }],
                ["set.value.function.typename", () => kv.set("k", function () {}),
                    { wantMsg: "got function" }],

                // --- value required (null/undefined) ---
                ["set.value.null", () => kv.set("k", null),
                    { wantMsg: "value must be provided" }],
                ["set.value.undefined", () => kv.set("k", undefined),
                    { wantMsg: "value must be provided" }],

                // --- incr options type errors (opt_number arms) ---
                ["incr.by.notnumber", () => kv.incr("k", { by: "x" }),
                    { wantMsg: "options.by must be a number" }],
                ["incr.by.notnumber.typename", () => kv.incr("k", { by: "x" }),
                    { wantMsg: "got string" }],
                ["incr.opts.notobject", () => kv.incr("k", 5),
                    { wantMsg: "options must be an object" }],
                ["incr.opts.notobject.typename", () => kv.incr("k", 5),
                    { wantMsg: "got number" }],
                ["incr.opts.boolean.typename", () => kv.incr("k", true),
                    { wantMsg: "got boolean" }],

                // --- set ttlMs option type error (opt_number via read_ttl_ms) ---
                ["set.ttlMs.notnumber", () => kv.set("k", "v", { ttlMs: "x" }),
                    { wantMsg: "options.ttlMs must be a number" }],

                // --- expire ttlMs not a number (dedicated expire arm) ---
                ["expire.ttlMs.notnumber", () => kv.expire("k", "x"),
                    { wantMsg: "ttlMs must be a number" }],
                ["expire.ttlMs.notnumber.typename", () => kv.expire("k", "x"),
                    { wantMsg: "got string" }],

                // --- list type errors (prefix + opts + cursor arms) ---
                ["list.prefix.notstring", () => kv.list(123),
                    { wantMsg: "prefix must be a string" }],
                ["list.prefix.notstring.typename", () => kv.list(123),
                    { wantMsg: "got number" }],
                ["list.opts.notobject", () => kv.list("p", "x"),
                    { wantMsg: "options must be an object" }],
                ["list.cursor.notstring", () => kv.list("p", { cursor: 5 }),
                    { wantMsg: "options.cursor must be a string" }],

                // --- invalid key (validate_key) ---
                ["key.empty", () => kv.get(""),
                    { wantMsg: "non-empty string" }],
                ["key.brace.open", () => kv.get("a{b"),
                    { wantMsg: "must not contain" }],
                ["key.brace.close", () => kv.get("a}b"),
                    { wantMsg: "must not contain" }],
                ["key.nul", () => kv.get("a" + String.fromCharCode(0) + "b"),
                    { wantMsg: "must not contain" }],
                ["key.control", () => kv.get("a" + String.fromCharCode(7) + "b"),
                    { wantMsg: "must not contain" }],
                ["key.toolong", () => kv.get("x".repeat(513)),
                    { wantMsg: "exceeds 512 bytes" }],

                // --- invalid value (size cap, validate_value) ---
                ["value.toolarge", () => kv.set("k", "x".repeat(300000)),
                    { wantMsg: "exceeds" }],

                // --- invalid delta (validate_delta) ---
                ["incr.by.fractional", () => kv.incr("k", { by: 1.5 }),
                    { wantMsg: "integer" }],
                ["incr.by.nan", () => kv.incr("k", { by: NaN }),
                    { wantMsg: "finite number" }],
                ["incr.by.outofrange", () => kv.incr("k", { by: 1e19 }),
                    { wantMsg: "out of i64 range" }],

                // --- invalid ttlMs (validate_ttl_ms) ---
                ["ttl.zero", () => kv.set("k", "v", { ttlMs: 0 }),
                    { wantMsg: "greater than 0" }],
                ["ttl.negative", () => kv.set("k", "v", { ttlMs: -1 }),
                    { wantMsg: "must not be negative" }],
                ["ttl.fractional", () => kv.set("k", "v", { ttlMs: 1.5 }),
                    { wantMsg: "must be an integer" }],
                ["ttl.toobig", () => kv.set("k", "v", { ttlMs: 1e30 }),
                    { wantMsg: "maximum" }],
                // expire path goes through the same validate_ttl_ms.
                ["expire.ttl.zero", () => kv.expire("k", 0),
                    { wantMsg: "greater than 0" }],

                // --- direct construction rejected (#[v8_constructor]) ---
                // `env.kv` is minted internally; calling the class
                // constructor directly must throw "Illegal constructor".
                ["construct.illegal", () => { new kv.constructor(); },
                    { wantMsg: "Illegal constructor" }],
            ];

            for (const [step, fn, opts] of cases) {
                const fail = await expectThrow(step, fn, opts);
                if (fail) {
                    return Response.json({ ok: false, ...fail, trace }, { status: 500 });
                }
            }

            return Response.json({ ok: true, count: cases.length, trace });
        } catch (e) {
            return Response.json({
                ok: false,
                step: trace.length ? trace[trace.length - 1] : "<none>",
                detail: "unexpected throw: " + String(e && e.message ? e.message : e),
                trace,
            }, { status: 500 });
        }
    }
};
"#;

// ---------------------------------------------------------------------------
// Backend edge-branch app — drives the "negative" backend results (missing
// key, no-TTL, already-present, empty prefix, single-page list) that the
// happy-path app doesn't reach. Backend-agnostic shape; redb runs it.
// ---------------------------------------------------------------------------
const KV_EDGE_APP: &str = r#"
export default {
    async fetch(request, env, ctx) {
        const kv = env.kv;
        const P = "edge:" + Math.random().toString(36).slice(2) + ":";
        const k = (s) => P + s;
        const trace = [];
        function fail(step, got, want) {
            const e = new Error("step failed: " + step);
            e.zsStep = step; e.zsGot = got; e.zsWant = want; throw e;
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
            // delete of a missing key → {deleted:false}
            {
                const d = await kv.delete(k("never"));
                eq("delete.missing.deleted", d && d.deleted, false);
            }
            // expire of a missing key → {updated:false}
            {
                const ex = await kv.expire(k("never"), 1000);
                eq("expire.missing.updated", ex && ex.updated, false);
            }
            // persist of a key with no TTL → {updated:false}
            {
                await kv.set(k("nottl"), "v");
                const pe = await kv.persist(k("nottl"));
                eq("persist.nottl.updated", pe && pe.updated, false);
            }
            // setIfAbsent when present → {stored:false}
            {
                const a = await kv.setIfAbsent(k("sia"), "a");
                eq("sia.first.stored", a && a.stored, true);
                const b = await kv.setIfAbsent(k("sia"), "b");
                eq("sia.second.stored", b && b.stored, false);
            }
            // ttl of a missing key → null
            {
                eq("ttl.missing.null", await kv.ttl(k("never")), null);
            }
            // list of an empty prefix range → {keys:[], cursor:null}
            {
                const res = await kv.list(k("emptyprefix:"));
                truthy("list.empty.keys.arr", res && Array.isArray(res.keys));
                eq("list.empty.keys.len", res.keys.length, 0);
                eq("list.empty.cursor", res.cursor, null);
            }
            // single-page list (fewer keys than limit) → cursor stays null
            {
                await kv.set(k("sp:1"), "x");
                await kv.set(k("sp:2"), "x");
                const res = await kv.list(k("sp:"), { limit: 100 });
                truthy("list.single.keys.arr", res && Array.isArray(res.keys));
                eq("list.single.keys.len", res.keys.length, 2);
                eq("list.single.cursor", res.cursor, null);
            }
            // list with no prefix arg → defaults to "" (list everything for app)
            {
                const res = await kv.list();
                truthy("list.noprefix.keys.arr", res && Array.isArray(res.keys));
                // at least the keys we set above are present
                truthy("list.noprefix.nonempty", res.keys.length >= 2);
            }

            return Response.json({ ok: true, trace });
        } catch (e) {
            if (e && e.zsStep) {
                return Response.json({
                    ok: false, step: e.zsStep,
                    got: e.zsGot === undefined ? null : e.zsGot,
                    want: e.zsWant === undefined ? null : e.zsWant,
                    trace,
                }, { status: 500 });
            }
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

// ---------------------------------------------------------------------------
// Realistic-scenario app — a fixed-window rate limiter and an ephemeral lock,
// both built only from the public `env.kv.*` surface. Doubles as end-to-end
// confidence that incr+ttl and setIfAbsent+delete compose correctly.
// ---------------------------------------------------------------------------
const KV_SCENARIO_APP: &str = r#"
export default {
    async fetch(request, env, ctx) {
        const kv = env.kv;
        const P = "scn:" + Math.random().toString(36).slice(2) + ":";
        const k = (s) => P + s;
        const trace = [];
        function fail(step, got, want) {
            const e = new Error("step failed: " + step);
            e.zsStep = step; e.zsGot = got; e.zsWant = want; throw e;
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
            // Fixed-window rate limiter: incr(window, {by:1, ttlMs}) N times.
            // The count increments 1..N and the TTL is set on create and stays
            // set across subsequent increments.
            {
                const win = k("rl:user42:window");
                for (let i = 1; i <= 5; i++) {
                    const n = await kv.incr(win, { by: 1, ttlMs: 60000 });
                    eq("rl.count." + i, n, i);
                    const t = await kv.ttl(win);
                    truthy("rl.ttl.set." + i,
                        t && typeof t.ttlMs === "number" && t.ttlMs > 0 && t.ttlMs <= 60000);
                }
            }

            // Ephemeral lock: setIfAbsent acquires; a second caller fails;
            // delete releases; re-acquire succeeds.
            {
                const lock = k("lock:job7");
                const a = await kv.setIfAbsent(lock, "owner-a", { ttlMs: 30000 });
                eq("lock.acquire", a && a.stored, true);
                const b = await kv.setIfAbsent(lock, "owner-b", { ttlMs: 30000 });
                eq("lock.contended", b && b.stored, false);
                eq("lock.owner", await kv.get(lock), "owner-a");
                const rel = await kv.delete(lock);
                eq("lock.release", rel && rel.deleted, true);
                const c = await kv.setIfAbsent(lock, "owner-c", { ttlMs: 30000 });
                eq("lock.reacquire", c && c.stored, true);
                eq("lock.newowner", await kv.get(lock), "owner-c");
            }

            return Response.json({ ok: true, trace });
        } catch (e) {
            if (e && e.zsStep) {
                return Response.json({
                    ok: false, step: e.zsStep,
                    got: e.zsGot === undefined ? null : e.zsGot,
                    want: e.zsWant === undefined ? null : e.zsWant,
                    trace,
                }, { status: 500 });
            }
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

// ---------------------------------------------------------------------------
// Backend-unavailable resilience app — every `env.kv.*` op MUST reject (NOT
// hang, NOT crash the isolate) when the configured Redis backend's server is
// absent. Each op is awaited inside try/catch; the catch records the rejection
// `.code`. The handler returns {ok:true} only if EVERY op rejected with a
// connection/backend code. A hang would never reach the return at all and the
// Rust harness's pump timeout would fail the test (surfaced, not silently
// passed).
//
// `kv_connection` is the canonical code for a connect/transport failure
// (single-node `pool()`→`Pool::connect` ECONNREFUSED and `conn()`→acquire both
// map to `KvError::connection`); `kv_backend` is accepted as a fallback so a
// differently-classified transport error still counts as a graceful reject.
// ---------------------------------------------------------------------------
const KV_BACKEND_DOWN_APP: &str = r#"
export default {
    async fetch(request, env, ctx) {
        const kv = env.kv;
        const trace = [];
        function fail(step, detail) {
            const e = new Error("step failed: " + step + " :: " + detail);
            e.zsStep = step; e.zsDetail = detail; throw e;
        }

        // Await a kv op that should REJECT because the backend is down.
        // Asserts it threw and that the coded error is a connection/backend
        // class code (the graceful-degradation contract). A non-reject
        // (resolve) is a failure; a hang never returns here at all.
        async function expectReject(step, fn) {
            trace.push(step);
            let threw = false;
            let code = null;
            let msg = null;
            try {
                await fn();
            } catch (e) {
                threw = true;
                code = e && e.code ? e.code : null;
                msg = e && e.message ? String(e.message) : null;
            }
            if (!threw) {
                fail(step, "op resolved but backend is down (expected reject)");
            }
            if (code !== "kv_connection" && code !== "kv_backend") {
                fail(step, "unexpected code=" + code + " msg=" + msg);
            }
            return code;
        }

        try {
            // Cover the distinct backend entry points: get (read), set
            // (write), incr (Lua EVAL path), list (SCAN path). Each routes
            // through conn()/pool() → connect → ECONNREFUSED → map_err.
            const codes = {};
            codes.get  = await expectReject("get",  () => kv.get("k"));
            codes.set  = await expectReject("set",  () => kv.set("k", "v"));
            codes.incr = await expectReject("incr", () => kv.incr("c"));
            codes.list = await expectReject("list", () => kv.list("p:"));

            return Response.json({ ok: true, codes, trace });
        } catch (e) {
            if (e && e.zsStep) {
                return Response.json({
                    ok: false, step: e.zsStep, detail: e.zsDetail, trace,
                }, { status: 500 });
            }
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
    run_app(backend, KV_E2E_APP)
}

/// Generalised harness: build a Runtime around `app` JS + `backend`, pump it,
/// call the fetch handler, and return `(status, body)`. `run_e2e` is the
/// happy-path specialisation; the validation / edge / scenario tests pass
/// their own app source.
fn run_app(backend: Arc<dyn Backend>, app: &'static str) -> (u16, String) {
    compio::runtime::Runtime::new().unwrap().block_on(async move {
        init_v8();

        // APP_ID flows through env_vars → build_instance reads it to scope
        // the per-app key namespace (see crates/runtime/src/core/plugin.rs).
        let mut env_vars = HashMap::new();
        env_vars.insert("APP_ID".to_string(), "e2e_app".to_string());

        let plugin: Arc<dyn NativePlugin> = Arc::new(KvPlugin::with_backend(backend));

        let runtime = Runtime::builder()
            .modules(module(app))
            .env_vars(env_vars)
            .plugins(vec![plugin])
            .build();
        runtime.start_pump();

        let env = EnvSnapshot::empty();
        let ctx = RequestCtx::new(CancelFlag::new());
        let outcome =
            runtime.call_fetch_handler("GET", "http://localhost/", &[], "", &env, ctx);

        match outcome {
            FetchOutcome::Response { status, body, .. } => {
                (status, String::from_utf8_lossy(&body).into_owned())
            }
            FetchOutcome::Pending { rx, cancel: _ } => {
                let settled = compio::time::timeout(Duration::from_secs(30), rx.recv())
                    .await
                    .expect("kv e2e: fetch pending timed out")
                    .expect("kv e2e: pending delivered DispatchError");
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

/// Open a fresh redb-backed backend in a tempdir. The `_dir` guard must
/// stay alive for the duration of the test (dropping it removes the file).
#[cfg(feature = "redb")]
fn redb_backend() -> (tempfile::TempDir, Arc<dyn Backend>) {
    let dir = tempfile::tempdir().expect("create tempdir");
    let path = dir.path().join("kv.redb");
    let backend = RedbBackend::open(&path).expect("open redb backend");
    (dir, Arc::new(backend))
}

/// Synchronous argument-validation throws (key shape, value type + size,
/// option ranges). These fire in the v8_class method body before any
/// dispatch, so they're backend-agnostic — redb runs them. Pins each
/// throw's message substring (and `.code` where applicable).
#[cfg(feature = "redb")]
#[test]
fn e2e_validation_errors() {
    let (_dir, backend) = redb_backend();
    let (status, body) = run_app(backend, KV_VALIDATION_APP);
    assert_ok(status, &body);
}

/// Backend "negative result" edge branches: delete/expire of a missing
/// key, persist of a no-TTL key, setIfAbsent when present, ttl of a
/// missing key, list of an empty prefix, and a single-page list whose
/// cursor stays null.
#[cfg(feature = "redb")]
#[test]
fn e2e_backend_edges() {
    let (_dir, backend) = redb_backend();
    let (status, body) = run_app(backend, KV_EDGE_APP);
    assert_ok(status, &body);
}

/// Realistic compositions over the public surface: a fixed-window rate
/// limiter (incr + ttlMs) and an ephemeral lock (setIfAbsent + delete).
#[cfg(feature = "redb")]
#[test]
fn e2e_scenarios() {
    let (_dir, backend) = redb_backend();
    let (status, body) = run_app(backend, KV_SCENARIO_APP);
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

// ---------------------------------------------------------------------------
// Backend UNAVAILABLE (single-node Redis pointed at a dead port) — runs in
// ANY environment because the point is that NO server is listening. This is
// the resilience guarantee: when the backend is down, app code calling
// `env.kv.*` rejects gracefully with a typed connection/backend error reaching
// JS — it does NOT hang and does NOT panic the isolate.
//
// It also exercises `redis.rs`'s connect-error `.map_err` arms (the bulk of
// its otherwise-uncovered lines): `pool()`→`Pool::connect` ECONNREFUSED, the
// `conn()`/acquire path, and `map_redis_err`'s connection-class mapping — none
// of which a healthy server ever triggers.
//
// 127.0.0.1:6398 is an unused high port (connection-refused on loopback is
// immediate, so this settles sub-second). The shared `run_app` harness drives
// the pump under a 30s `compio::time::timeout`: a failure to reject (a hang)
// would blow that timeout and FAIL the test rather than hang forever.
#[cfg(feature = "redis")]
#[test]
fn e2e_backend_unavailable() {
    // Single-node URL (NOT cluster) at a dead port — nothing listens here.
    let backend = Redis::new("redis://127.0.0.1:6398");
    let (status, body) = run_app(Arc::new(backend), KV_BACKEND_DOWN_APP);
    assert_ok(status, &body);
}

// ===========================================================================
// `env.meter` is GONE (Refactor A): metering is infrastructure, so there is
// no creator-facing `env.meter` namespace. App code must observe it as
// `undefined` — it cannot self-report (forge/suppress) billing.
// ===========================================================================

/// Faithful: build a real Runtime with the kv plugin (a representative app
/// kernel) and assert from inside the isolate that `env.meter` is undefined
/// and not callable. RED before Refactor A (when `MeterPlugin` registered
/// the `meter` namespace), GREEN after the deletion.
#[cfg(feature = "redb")]
#[test]
fn env_meter_namespace_is_absent_from_app_code() {
    const APP: &str = r#"
export default {
    async fetch(request, env, ctx) {
        const present = typeof env.meter !== "undefined";
        let calledOk = false;
        try { env.meter.increment("x"); calledOk = true; } catch (_) { /* expected */ }
        // ok iff env.meter is undefined AND there is no working increment.
        return Response.json({ ok: !present && !calledOk, present, calledOk });
    },
};
"#;
    let dir = tempfile::tempdir().expect("tempdir");
    let backend = Arc::new(
        RedbBackend::open(dir.path().join("kv.redb")).expect("open redb"),
    );
    // Use the production-shaped constructor (with a meter) to prove that even
    // when the worker HAS a meter, no `env.meter` surface is exposed.
    let (status, body, _meter) =
        run_app_metered(backend, APP, "00000000-0000-7000-8000-0000000000f6");
    assert_eq!(status, 200, "env.meter probe non-200; body: {body}");
    assert!(
        body.contains(r#""ok":true"#),
        "env.meter must be undefined and uncallable from app code; body: {body}"
    );
}

// ===========================================================================
// Metering-as-infrastructure (Refactor A): each kv op emits a raw usage
// metric (kv_reads / kv_writes) into the process-wide Meter at its op
// boundary, scoped to the server-injected APP_ID. App code can neither forge
// nor suppress these — they are emitted by trusted Rust inside the primitive,
// not via a creator-facing `env.meter` API (which no longer exists).
// ===========================================================================

/// Build a Runtime around `app` + `backend` + a real Meter bound to
/// `app_id`, pump it, run the fetch handler, and return
/// `(status, body, meter)` so the test can drain what the kv ops recorded.
/// Faithful: drives the REAL `KvPlugin::with_backend_and_meter` →
/// `build_instance` → `mint_kv` → `dispatch_*` path an app sees.
fn run_app_metered(
    backend: Arc<dyn Backend>,
    app: &'static str,
    app_id: &str,
) -> (u16, String, Arc<zeroship_metering::Meter>) {
    let meter = Arc::new(zeroship_metering::Meter::new());
    let meter_for_run = Arc::clone(&meter);
    let app_id = app_id.to_string();
    let (status, body) = compio::runtime::Runtime::new().unwrap().block_on(async move {
        init_v8();

        let mut env_vars = HashMap::new();
        env_vars.insert("APP_ID".to_string(), app_id.clone());

        let plugin: Arc<dyn NativePlugin> =
            Arc::new(KvPlugin::with_backend_and_meter(backend, Some(meter_for_run)));

        let runtime = Runtime::builder()
            .modules(module(app))
            .env_vars(env_vars)
            .plugins(vec![plugin])
            .build();
        runtime.start_pump();

        let env = EnvSnapshot::empty();
        let ctx = RequestCtx::new(CancelFlag::new());
        let outcome =
            runtime.call_fetch_handler("GET", "http://localhost/", &[], "", &env, ctx);

        match outcome {
            FetchOutcome::Response { status, body, .. } => {
                (status, String::from_utf8_lossy(&body).into_owned())
            }
            FetchOutcome::Pending { rx, cancel: _ } => {
                let settled = compio::time::timeout(Duration::from_secs(30), rx.recv())
                    .await
                    .expect("kv metering: fetch pending timed out")
                    .expect("kv metering: pending delivered DispatchError");
                match settled {
                    SettledFetch::Response { status, body, .. } => {
                        (status, String::from_utf8_lossy(&body).into_owned())
                    }
                    _ => panic!("kv metering: expected SettledFetch::Response"),
                }
            }
            _ => panic!("kv metering: unexpected outcome"),
        }
    });
    (status, body, meter)
}

/// 3 writes (set, set, incr) + 2 reads (get, get-miss) — the handler returns
/// ok:true. APP_ID is a real UUID so `Meter::drain` (which keys by parsed
/// UUID) surfaces the exact per-app counts.
#[cfg(feature = "redb")]
const KV_METER_APP: &str = r#"
export default {
    async fetch(request, env, ctx) {
        const kv = env.kv;
        const P = "meter:" + Math.random().toString(36).slice(2) + ":";
        await kv.set(P + "a", "1");       // write
        await kv.set(P + "b", "2");       // write
        await kv.incr(P + "c");           // write
        await kv.get(P + "a");            // read
        await kv.get(P + "missing");      // read (miss still bills a read op)
        return Response.json({ ok: true });
    },
};
"#;

#[cfg(feature = "redb")]
#[test]
fn metering_kv_ops_counts_are_exact_and_per_app() {
    let dir = tempfile::tempdir().expect("tempdir");
    let backend = Arc::new(
        RedbBackend::open(dir.path().join("kv.redb")).expect("open redb"),
    );
    let app_id = "00000000-0000-7000-8000-0000000000a1";
    let (status, body, meter) = run_app_metered(backend, KV_METER_APP, app_id);
    assert_ok(status, &body);

    let snap = meter.drain();
    let id = uuid::Uuid::parse_str(app_id).unwrap();
    let usage = snap.get(&id).expect("meter recorded usage for the app");
    assert_eq!(
        usage.custom.get("kv_writes").copied(),
        Some(3),
        "set + set + incr = 3 kv_writes; got {:?}",
        usage.custom
    );
    assert_eq!(
        usage.custom.get("kv_reads").copied(),
        Some(2),
        "get + get(miss) = 2 kv_reads; got {:?}",
        usage.custom
    );
}

/// A FAILED kv op must NOT emit a metric — billing only on success. Drive kv
/// ops against a DOWN backend (dead Redis port): every op rejects, so the
/// Meter stays empty for this app. Faithful: same dispatch path, real failure
/// arm.
#[cfg(feature = "redis")]
#[test]
fn metering_failed_kv_op_emits_nothing() {
    const APP: &str = r#"
export default {
    async fetch(request, env, ctx) {
        try { await env.kv.set("k", "v"); } catch (e) { /* expected: backend down */ }
        try { await env.kv.get("k"); } catch (e) { /* expected */ }
        return Response.json({ ok: true });
    },
};
"#;
    // Dead port → every op rejects.
    let backend: Arc<dyn Backend> = Arc::new(Redis::new("redis://127.0.0.1:6398"));
    let app_id = "00000000-0000-7000-8000-0000000000b2";
    let (status, body, meter) = run_app_metered(backend, APP, app_id);
    assert_ok(status, &body);

    let snap = meter.drain();
    let id = uuid::Uuid::parse_str(app_id).unwrap();
    // No successful op ⇒ no metric for this app at all (drain omits zero apps).
    assert!(
        snap.get(&id).is_none(),
        "a failed kv op must emit no metric; got {:?}",
        snap.get(&id)
    );
}
