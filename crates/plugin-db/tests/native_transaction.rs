//! P9 PR 3 — end-to-end tests for the native `Db.transaction(fn)`
//! orchestrator against real Postgres.
//!
//! These dispatch through a real `Runtime` + the `DbPlugin` and call
//! `env.db.transaction(async tx => {...})` directly — the native
//! v8_method, not the bootstrap wrapper. They exercise the full
//! Rust→V8→Rust flow the orchestrator relies on:
//!
//!   begin (spawned op) → Continuation (mint tx-view, call callback,
//!   attach .then) → commit/rollback handler (spawned op) → settle outer.
//!
//! Scenarios:
//!   - commit on resolve (row persists, visible afterwards);
//!   - rollback on async reject (no row persists);
//!   - rollback on synchronous throw inside the callback;
//!   - nested transaction emits SAVEPOINT — inner reject rolls back to
//!     the savepoint while the outer continues + commits;
//!   - nested transaction inner resolve releases the savepoint (both
//!     writes persist);
//!   - savepoint depth cap (9th level → `savepoint_depth_exceeded`);
//!   - tx-view is collections-only (no `commit`/`rollback`).
//!
//! Requires: `docker start pg-test` (Postgres on port 5434).
//! Run: `cargo test -p zeroship-plugin-db --test native_transaction -- --test-threads=1`
//!
//! NOTE (baseline): every test here runs `registerModel("notes", ...)`
//! in `setup`. On a Postgres that enforces the
//! single-command-per-extended-query rule, that DDL apply
//! fails with `cannot insert multiple commands into a prepared statement`
//! (the collection's `CREATE TABLE` + implicit `CREATE INDEX` payload is
//! issued via `pool_exec`'s extended protocol — a pre-existing DDL-layer
//! issue, NOT specific to transactions). On such an instance these tests
//! fail at `setup`. The orchestrator logic itself is covered without a
//! DB by the Rust unit tests + tx-view shape
//! tests in `crates/plugin-db/src/{orchestrator,v8_classes}/transaction.rs`,
//! the `db_v8_class.rs` surface tests, the SQLite SAVEPOINT SQL tests in
//! `sqlite_integration.rs`, and the SDK-side mock tests in
//! `sdks/db/tests/p9-pr3-native-transaction.test.ts`.

use std::sync::Arc;
use std::time::Duration;

use compio_postgres::NoTls;
use zeroship_plugin_db::DbPlugin;
use zeroship_runtime::channel::CancelFlag;
use zeroship_runtime::plugin::NativePlugin;
use zeroship_runtime::runtime::Runtime;
use zeroship_runtime::{init_v8, EnvSnapshot, FetchOutcome, ModuleEntry, RequestCtx, SettledFetch};

fn pg_url() -> String {
    std::env::var("PG_TEST_URL")
        .unwrap_or_else(|_| "postgres://postgres:test@localhost:5434/postgres".to_string())
}

fn require_pg_or_skip() -> Option<String> {
    let url = pg_url();
    let url_clone = url.clone();
    let ok = compio::runtime::Runtime::new().unwrap().block_on(async move {
        match compio_postgres::connect(&url_clone, NoTls).await {
            Ok((client, connection)) => {
                compio::runtime::spawn(async move {
                    let _ = connection.run().await;
                })
                .detach();
                drop(client);
                true
            }
            Err(_) => false,
        }
    });
    if ok { Some(url) } else { None }
}

/// plugin-db's registerModel uses env_vars.APP_ID for the schema name
/// (defaults to "default" when unset). Tests run with the empty
/// EnvSnapshot so we target "default".
const APP_SCHEMA: &str = "default";

fn reset_schema(url: &str) {
    let url = url.to_string();
    compio::runtime::Runtime::new().unwrap().block_on(async move {
        let (client, connection) = compio_postgres::connect(&url, NoTls).await.unwrap();
        compio::runtime::spawn(async move {
            let _ = connection.run().await;
        })
        .detach();
        client
            .execute(&format!("DROP SCHEMA IF EXISTS \"{APP_SCHEMA}\" CASCADE"), &[])
            .await
            .unwrap();
    });
}

fn count_notes(url: &str) -> i64 {
    let url = url.to_string();
    compio::runtime::Runtime::new().unwrap().block_on(async move {
        let (client, connection) = compio_postgres::connect(&url, NoTls).await.unwrap();
        compio::runtime::spawn(async move {
            let _ = connection.run().await;
        })
        .detach();
        let sql = format!("SELECT COUNT(*)::bigint AS c FROM \"{APP_SCHEMA}\".\"notes\"");
        let rows = client.query(&sql, &[]).await.unwrap();
        rows[0].get::<_, i64>("c")
    })
}

/// Self-contained dispatcher shim: a function-shape `default.rpc` that
/// looks up `_procedures[name]` and runs it. These tests open
/// transactions explicitly via `env.db.transaction`.
const SHIM: &str = r#"
async function _shimRpc(name, input, ctx) {
    const fn = _procedures[name];
    if (typeof fn !== "function") {
        throw Object.assign(new Error("Method not found: " + name), { status: 404 });
    }
    let out = fn(input, ctx);
    if (out && typeof out.then === "function") out = await out;
    return out;
}
async function _zsRpcAndRespond(name, input) {
    try {
        const result = await _shimRpc(name, input);
        return new Response(JSON.stringify({ json: result === undefined ? null : result }),
            { status: 200, headers: { "content-type": "application/json" } });
    } catch (err) {
        const status = (err && Number.isInteger(err.status) && err.status >= 400 && err.status < 600) ? err.status : 500;
        const body = { message: err?.message ?? String(err), name: err?.name ?? "Error" };
        if (err && typeof err.code === "string") body.code = err.code;
        return new Response(JSON.stringify(body), {
            status, headers: { "content-type": "application/json" },
        });
    }
}
async function _zsFetch(request) {
    const url = new URL(request.url);
    const id = decodeURIComponent(url.pathname.slice("/__zeroship/v1/".length));
    const text = await request.text();
    let input;
    if (text) {
        const env = JSON.parse(text);
        input = env && typeof env === "object" && "json" in env ? env.json : env;
    }
    return await _zsRpcAndRespond(id, input);
}
export default { fetch: _zsFetch, rpc: _shimRpc };
"#;

fn dispatch_zs(url: &str, source: &str, name: &str) -> (u16, serde_json::Value) {
    init_v8();
    let modules = vec![ModuleEntry {
        specifier: "index.js".into(),
        source: source.into(),
    }];
    let plugins: Vec<Arc<dyn NativePlugin>> = vec![Arc::new(DbPlugin::new(url.to_string(), None))];
    let runtime = Runtime::builder().modules(modules).plugins(plugins).build();
    let env = EnvSnapshot::empty();
    let ctx = RequestCtx::new(CancelFlag::new());
    let url_ep = format!("http://localhost/__zeroship/v1/{name}");
    let outcome = runtime.call_fetch_handler(
        "POST",
        &url_ep,
        &[("content-type".into(), "application/json".into())],
        r#"{"json":null}"#,
        &env,
        ctx,
    );
    let (status, body) = match outcome {
        FetchOutcome::Response { status, body, .. } => (status, body),
        FetchOutcome::Pending { rx, cancel: _ } => {
            compio::runtime::Runtime::new().unwrap().block_on(async {
                runtime.start_pump();
                let settled = compio::time::timeout(Duration::from_secs(15), rx.recv())
                    .await
                    .expect("pending timeout")
                    .expect("settled error");
                match settled {
                    SettledFetch::Response { status, body, .. } => (status, body),
                    _ => panic!("expected Response variant"),
                }
            })
        }
        _ => panic!("unexpected outcome variant"),
    };
    let json: serde_json::Value =
        serde_json::from_str(&body).unwrap_or(serde_json::Value::String(body.clone()));
    (status, json)
}

/// `_procedures` declaring `setup` (registerModel) plus the per-test
/// handlers `body`.
///
/// **P9 PR 4** — `registerModel` moved off `env.db` to the `__platform`
/// capability handle (reached via `globalThis.__zsDbPlatform`, which the
/// production runtime-entry DELETES before request handlers run). This
/// test module's top-level evaluates BEFORE that deletion (ESM import
/// hoisting puts `__user__.js` ahead of the spliced runtime-entry), so we
/// capture the resolver into a module-local const here and register
/// through it — mirroring how `@zeroship/bootstrap`'s dev-entry captures
/// the handle at module init.
fn build_src(body: &str) -> String {
    format!(
        r#"
import {{ env }} from "zeroship";

const __plat = (typeof globalThis.__zsDbPlatform === "function")
    ? globalThis.__zsDbPlatform(env.db)
    : undefined;

function setup(_input, _ctx) {{
    return __plat.registerModel("notes", {{
        title: {{ type: "string", required: true }},
    }});
}}
setup.config = {{ kind: "action" }};

{body}
"#
    ) + SHIM
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

/// Commit on resolve: the callback inserts a row and resolves; the row
/// persists and is visible after the transaction commits.
#[test]
fn transaction_commits_on_resolve() {
    let Some(url) = require_pg_or_skip() else { return };
    reset_schema(&url);

    let src = build_src(
        r#"
async function commitOne(_input, _ctx) {
    const r = await env.db.transaction(async (tx) => {
        await tx.notes.insert({ title: "committed" });
        return "ok";
    });
    return { txResult: r };
}
commitOne.config = { kind: "action" };
const _procedures = { setup, commitOne };
"#,
    );

    let (status, body) = dispatch_zs(&url, &src, "setup");
    assert_eq!(status, 200, "setup failed: {body}");

    let (status, body) = dispatch_zs(&url, &src, "commitOne");
    assert_eq!(status, 200, "commitOne failed: {body}");
    let inner = body.get("json").cloned().unwrap_or(serde_json::Value::Null);
    assert_eq!(
        inner.get("txResult").and_then(|v| v.as_str()),
        Some("ok"),
        "transaction(fn) must resolve with the callback's return value; body={body}"
    );
    assert_eq!(count_notes(&url), 1, "committed row must be visible");
}

/// Rollback on async reject: the callback inserts then throws; nothing
/// persists, and `transaction(fn)` rejects with the thrown error.
#[test]
fn transaction_rolls_back_on_async_reject() {
    let Some(url) = require_pg_or_skip() else { return };
    reset_schema(&url);

    let src = build_src(
        r#"
async function insertThenThrow(_input, _ctx) {
    try {
        await env.db.transaction(async (tx) => {
            await tx.notes.insert({ title: "rolledBack" });
            throw Object.assign(new Error("abort it"), { code: "user_abort" });
        });
        return { reached: "no-throw" };
    } catch (e) {
        return { caught: e.message, code: e.code ?? null };
    }
}
insertThenThrow.config = { kind: "action" };
const _procedures = { setup, insertThenThrow };
"#,
    );

    let (status, _b) = dispatch_zs(&url, &src, "setup");
    assert_eq!(status, 200);

    let (status, body) = dispatch_zs(&url, &src, "insertThenThrow");
    assert_eq!(status, 200, "harness should succeed: {body}");
    let inner = body.get("json").cloned().unwrap_or(serde_json::Value::Null);
    assert_eq!(
        inner.get("caught").and_then(|v| v.as_str()),
        Some("abort it"),
        "transaction(fn) must reject with the thrown error verbatim; body={body}"
    );
    assert_eq!(
        inner.get("code").and_then(|v| v.as_str()),
        Some("user_abort"),
        "the thrown error's custom .code must survive the rollback path; body={body}"
    );
    assert_eq!(count_notes(&url), 0, "rolled-back tx must not persist");
}

/// Rollback on a synchronous throw inside the callback (the callback is
/// not `async` and throws before returning a Promise — captured by the
/// orchestrator's TryCatch).
#[test]
fn transaction_sync_throw_in_callback_rolls_back() {
    let Some(url) = require_pg_or_skip() else { return };
    reset_schema(&url);

    let src = build_src(
        r#"
async function syncThrow(_input, _ctx) {
    try {
        await env.db.transaction((tx) => {
            // Not async — throws synchronously before any Promise exists.
            throw new Error("sync boom");
        });
        return { reached: "no-throw" };
    } catch (e) {
        return { caught: e.message };
    }
}
syncThrow.config = { kind: "action" };
const _procedures = { setup, syncThrow };
"#,
    );

    let (status, _b) = dispatch_zs(&url, &src, "setup");
    assert_eq!(status, 200);

    let (status, body) = dispatch_zs(&url, &src, "syncThrow");
    assert_eq!(status, 200, "harness should succeed: {body}");
    let inner = body.get("json").cloned().unwrap_or(serde_json::Value::Null);
    assert_eq!(
        inner.get("caught").and_then(|v| v.as_str()),
        Some("sync boom"),
        "a synchronous throw inside the callback must reject transaction(fn); body={body}"
    );
    assert_eq!(count_notes(&url), 0);
}

/// Nested transaction emits a SAVEPOINT: the inner `transaction()` fails,
/// rolling back ONLY its own write (ROLLBACK TO SAVEPOINT); the outer
/// catches the inner failure, inserts its own row, and commits. Only the
/// outer row persists — proving the savepoint isolated the inner failure
/// without poisoning the whole tx.
#[test]
fn nested_inner_reject_rolls_back_to_savepoint_outer_continues() {
    let Some(url) = require_pg_or_skip() else { return };
    reset_schema(&url);

    let src = build_src(
        r#"
async function nestedPartialFailure(_input, _ctx) {
    const r = await env.db.transaction(async (tx) => {
        // Inner tx writes then fails — only the inner write reverts.
        try {
            await env.db.transaction(async (tx2) => {
                await tx2.notes.insert({ title: "inner-doomed" });
                throw new Error("inner abort");
            });
        } catch (e) {
            // swallow — the outer continues
        }
        // After the inner ROLLBACK TO SAVEPOINT, the outer tx is still
        // alive (not poisoned) and can keep writing.
        await tx.notes.insert({ title: "outer-survives" });
        return "outer-committed";
    });
    return { txResult: r };
}
nestedPartialFailure.config = { kind: "action" };
const _procedures = { setup, nestedPartialFailure };
"#,
    );

    let (status, _b) = dispatch_zs(&url, &src, "setup");
    assert_eq!(status, 200);

    let (status, body) = dispatch_zs(&url, &src, "nestedPartialFailure");
    assert_eq!(status, 200, "nested handler should succeed: {body}");
    let inner = body.get("json").cloned().unwrap_or(serde_json::Value::Null);
    assert_eq!(
        inner.get("txResult").and_then(|v| v.as_str()),
        Some("outer-committed"),
        "outer tx must commit after the inner savepoint rolled back; body={body}"
    );
    // Exactly the outer row persists — the inner one was rolled back to
    // the savepoint, the outer one committed.
    assert_eq!(
        count_notes(&url),
        1,
        "only the outer row must persist (inner rolled back to SAVEPOINT); body={body}"
    );
}

/// Nested transaction inner resolve releases the savepoint: both the
/// inner and outer writes commit (RELEASE SAVEPOINT then top-level
/// COMMIT).
#[test]
fn nested_inner_resolve_releases_savepoint() {
    let Some(url) = require_pg_or_skip() else { return };
    reset_schema(&url);

    let src = build_src(
        r#"
async function nestedBothCommit(_input, _ctx) {
    const r = await env.db.transaction(async (tx) => {
        await env.db.transaction(async (tx2) => {
            await tx2.notes.insert({ title: "inner-kept" });
            return "inner-ok";
        });
        await tx.notes.insert({ title: "outer-kept" });
        return "both-ok";
    });
    return { txResult: r };
}
nestedBothCommit.config = { kind: "action" };
const _procedures = { setup, nestedBothCommit };
"#,
    );

    let (status, _b) = dispatch_zs(&url, &src, "setup");
    assert_eq!(status, 200);

    let (status, body) = dispatch_zs(&url, &src, "nestedBothCommit");
    assert_eq!(status, 200, "nested handler should succeed: {body}");
    let inner = body.get("json").cloned().unwrap_or(serde_json::Value::Null);
    assert_eq!(
        inner.get("txResult").and_then(|v| v.as_str()),
        Some("both-ok")
    );
    assert_eq!(
        count_notes(&url),
        2,
        "both inner (RELEASE SAVEPOINT) and outer (COMMIT) writes must persist; body={body}"
    );
}

/// Savepoint depth cap: nesting `transaction()` past MAX_SAVEPOINT_DEPTH
/// (8) rejects the 9th level with `savepoint_depth_exceeded`.
#[test]
fn savepoint_depth_cap_8_exceeded_throws() {
    let Some(url) = require_pg_or_skip() else { return };
    reset_schema(&url);

    // Recurse `transaction()` to depth `n`. The outermost is the BEGIN
    // (depth 0 savepoints); each nested call is one savepoint. The 9th
    // *savepoint* (i.e. the 10th transaction() level) must throw. We open
    // 1 BEGIN + N savepoints; choosing N = 9 trips the cap.
    let src = build_src(
        r#"
async function deepNest(_input, _ctx) {
    let level = 0;
    async function go() {
        level += 1;
        if (level > 12) return; // safety stop (should never reach)
        await env.db.transaction(async () => {
            await go();
        });
    }
    try {
        await go();
        return { tripped: false, reachedLevel: level };
    } catch (e) {
        return { tripped: true, code: e.code ?? null, reachedLevel: level };
    }
}
deepNest.config = { kind: "action" };
const _procedures = { setup, deepNest };
"#,
    );

    let (status, _b) = dispatch_zs(&url, &src, "setup");
    assert_eq!(status, 200);

    let (status, body) = dispatch_zs(&url, &src, "deepNest");
    // The deepest reject propagates up through every level (each inner
    // rejection rolls its savepoint back and re-rejects), so the
    // outermost transaction(fn) rejects — the handler catches it.
    assert_eq!(status, 200, "handler should catch the depth-cap error: {body}");
    let inner = body.get("json").cloned().unwrap_or(serde_json::Value::Null);
    assert_eq!(
        inner.get("tripped").and_then(|v| v.as_bool()),
        Some(true),
        "deep nesting must trip the savepoint depth cap; body={body}"
    );
    assert_eq!(
        inner.get("code").and_then(|v| v.as_str()),
        Some("savepoint_depth_exceeded"),
        "the depth-cap rejection must carry code=savepoint_depth_exceeded; body={body}"
    );
}

/// The `tx` view handed to the callback is collections-only: it has no
/// `commit` / `rollback` / `collection` method.
#[test]
fn tx_view_has_no_lifecycle_methods() {
    let Some(url) = require_pg_or_skip() else { return };
    reset_schema(&url);

    let src = build_src(
        r#"
async function probeTxView(_input, _ctx) {
    return await env.db.transaction(async (tx) => {
        return {
            hasCommit: typeof tx.commit,
            hasRollback: typeof tx.rollback,
            hasCollection: typeof tx.collection,
            hasNotes: typeof tx.notes,        // a collection prop IS present
        };
    });
}
probeTxView.config = { kind: "action" };
const _procedures = { setup, probeTxView };
"#,
    );

    let (status, _b) = dispatch_zs(&url, &src, "setup");
    assert_eq!(status, 200);

    let (status, body) = dispatch_zs(&url, &src, "probeTxView");
    assert_eq!(status, 200, "probe should succeed: {body}");
    let inner = body.get("json").cloned().unwrap_or(serde_json::Value::Null);
    assert_eq!(
        inner.get("hasCommit").and_then(|v| v.as_str()),
        Some("undefined"),
        "tx.commit must not exist; body={body}"
    );
    assert_eq!(
        inner.get("hasRollback").and_then(|v| v.as_str()),
        Some("undefined"),
        "tx.rollback must not exist; body={body}"
    );
    assert_eq!(
        inner.get("hasCollection").and_then(|v| v.as_str()),
        Some("undefined"),
        "tx.collection must not exist; body={body}"
    );
    assert_eq!(
        inner.get("hasNotes").and_then(|v| v.as_str()),
        Some("object"),
        "tx.notes (a tx-bound collection) must be present; body={body}"
    );
}

/// `env.db.beginTransaction` is gone from the runtime surface — a handler
/// reading it sees `undefined`.
#[test]
fn begin_transaction_not_on_env_db() {
    let Some(url) = require_pg_or_skip() else { return };
    reset_schema(&url);

    let src = build_src(
        r#"
function probeBegin(_input, _ctx) {
    return { beginType: typeof env.db.beginTransaction };
}
probeBegin.config = { kind: "action" };
const _procedures = { setup, probeBegin };
"#,
    );

    let (status, _b) = dispatch_zs(&url, &src, "setup");
    assert_eq!(status, 200);

    let (status, body) = dispatch_zs(&url, &src, "probeBegin");
    assert_eq!(status, 200, "probe should succeed: {body}");
    let inner = body.get("json").cloned().unwrap_or(serde_json::Value::Null);
    assert_eq!(
        inner.get("beginType").and_then(|v| v.as_str()),
        Some("undefined"),
        "env.db.beginTransaction must be undefined (deleted in P9 PR 3); body={body}"
    );
}
