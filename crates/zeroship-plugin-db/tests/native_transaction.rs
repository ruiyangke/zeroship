//! End-to-end tests for the native `Db.transaction(fn)`
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
//! Requires: the test PostgreSQL named by the overlay
//! (`deploy/ops/zeroship.test.toml`, written by
//! `tests/provision_test_backends.sh`) or by `PG_TEST_URL`. There is no
//! compiled default; see `crates/core/src/config/test_overlay.rs`.
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

mod support;

use std::sync::Arc;
use std::time::Duration;

use compio_postgres::NoTls;
use zeroship_plugin_db::service::{DbService, DbServiceConfig};
use zeroship_runtime::channel::CancelFlag;
use zeroship_runtime::plugin::NativePlugin;
use zeroship_runtime::runtime::Runtime;
use zeroship_runtime::{init_v8, EnvSnapshot, FetchOutcome, ModuleEntry, RequestCtx, SettledFetch};

fn pg_url() -> String {
    zeroship_core::config::test_database_url()
}

thread_local! {
    /// ONE compio runtime per test thread, alive for the whole thread.
    ///
    /// plugin-db parks its `compio_postgres::Pool` (and the backend handle
    /// wrapping it) in a **thread-local** `ThreadDbContext` that deliberately
    /// outlives any single dispatch — `DbPlugin::register` only clears the pool
    /// when the DB URL *changes*, and every dispatch here uses the same URL. A
    /// compio runtime built per dispatch and dropped at the end of it therefore
    /// leaves that cached pool holding sockets registered with a driver that no
    /// longer exists: the next dispatch's first pooled query is submitted to a
    /// dead io_uring and never completes. That is the `pending timeout` this
    /// harness used to hit — and it is not specific to transactions at all
    /// (a plain `env.db.collection("notes").insert(...)` in a second dispatch
    /// hangs identically).
    ///
    /// Production has exactly one compio runtime per worker thread for the
    /// process lifetime, so tying the runtime's lifetime to the thread — the
    /// same scope the DB context already uses — is both the faithful shape and
    /// the fix.
    ///
    /// V8 is still entered OUTSIDE any compio runtime: `block_on` enters the
    /// runtime only for the duration of the future it drives, and
    /// `call_fetch_handler` runs before that call. Do not "unify" the two by
    /// moving the V8 entry inside `block_on`.
    static RT: compio::runtime::Runtime =
        compio::runtime::Runtime::new().expect("build the per-thread compio runtime");
}

/// Drive `fut` on this thread's long-lived compio runtime.
fn block_on<F: std::future::Future>(fut: F) -> F::Output {
    RT.with(|rt| rt.block_on(fut))
}

/// Connects, or fails the test.
///
/// Deliberately NOT a skip. This binary is already opt-in behind
/// `required-features = ["test-helpers"]`, so reaching here means someone
/// asked for the live-Postgres suite; answering "8 passed" without a database
/// tells them the opposite of the truth. These four transaction tests were
/// broken for a long time behind exactly that green, and a skipping run is
/// indistinguishable from a passing one at a glance - only the clock differs
/// (0.02s against nothing, ~11s against Postgres).
fn require_pg() -> String {
    // Every test in this binary funnels through here, so this is the one place
    // that has to install the subscriber. Without it the runtime's sanitization
    // rail leaves a failure as a bare `{"message":"internal error"}` and the
    // real cause goes to a discarded tracing stream. No-op unless RUST_LOG is
    // set. See `support::init_test_tracing`.
    support::init_test_tracing();
    let url = pg_url();
    let url_clone = url.clone();
    let ok = block_on(async move {
        match compio_postgres::connect(&url_clone, NoTls).await {
            Ok((client, connection)) => {
                compio::runtime::spawn(async move {
                    let _ = connection.run().await;
                })
                .detach();
                drop(client);
                drain_open_connections().await;
                true
            }
            Err(_) => false,
        }
    });
    assert!(
        ok,
        "native_transaction needs a live Postgres at {url}. Set PG_TEST_URL to \
         override. This suite is opt-in, so it fails rather than skipping: a \
         skipped run reports the same \"ok\" as a passing one."
    );
    url
}

/// plugin-db's registerModel uses env_vars.APP_ID for the schema name
/// (defaults to "default" when unset). Tests run with the empty
/// EnvSnapshot so we target "default".
const APP_SCHEMA: &str = "default";

/// Drop the app schema, then PROVISION the `notes` table the way the engine /
/// deploy-apply does.
///
/// `registerModel` on the PG dialect issues NO runtime
/// DDL — `zeroship-migrate` is the sole PG schema authority and creates the
/// schema at deploy time, before the app serves. So the table must already
/// exist when the handlers run. We stand in for the deploy-time engine apply by
/// building the schema via `exec_register_model_with_pool` (the same DDL +
/// sentinel emission the relocated engine produces), then the handlers'
/// `registerModel` call is a faithful no-op — exactly the production order
/// (engine-at-deploy, runtime-reads-only).
/// Release this runtime's connections before it falls out of scope.
///
/// Every helper here builds its own runtime and drops it when the block ends.
/// Closing a connection is asynchronous, so a runtime that stops first leaves
/// the socket - and the server-side backend - alive for the life of the test
/// process. Draining inside the block keeps the runtime alive long enough to
/// finish the close.
async fn drain_open_connections() {
    zeroship_plugin_db::reset_context_for_tests();
    if !compio_postgres::drain_connections(std::time::Duration::from_secs(2)).await {
        eprintln!(
            "DRAIN-TIMEOUT: {} connection(s) still live",
            compio_postgres::live_connections()
        );
    }
}

fn reset_schema(url: &str) {
    let url = url.to_string();
    block_on(async move {
        let (client, connection) = compio_postgres::connect(&url, NoTls).await.unwrap();
        compio::runtime::spawn(async move {
            let _ = connection.run().await;
        })
        .detach();
        client
            .execute(&format!("DROP SCHEMA IF EXISTS \"{APP_SCHEMA}\" CASCADE"), &[])
            .await
            .unwrap();
        drop(client);

        // Deploy stand-in, as RAW SQL. `zeroship-migrate` is the PG schema
        // authority; registerModel applies no DDL, so a test that needs the
        // `notes` table creates it. The per-isolate context still needs the URL
        // for the transaction orchestrator under test.
        zeroship_plugin_db::set_db_url_for_tests(&url);
        let pool = std::rc::Rc::new(compio_postgres::Pool::connect(&url, 2).await.unwrap());
        pool.batch_execute(&format!(
            r#"CREATE SCHEMA IF NOT EXISTS "{APP_SCHEMA}";
CREATE TABLE "{APP_SCHEMA}"."notes" (
  id TEXT PRIMARY KEY,
  created_at TIMESTAMPTZ NOT NULL DEFAULT NOW(),
  updated_at TIMESTAMPTZ NOT NULL DEFAULT NOW(),
  created_by TEXT NULL,
  updated_by TEXT NULL,
  version INTEGER NOT NULL DEFAULT 1,
  deleted_at TIMESTAMPTZ NULL,
  "title" TEXT NOT NULL
);
CREATE INDEX IF NOT EXISTS "notes_deleted_at_idx" ON "{APP_SCHEMA}"."notes" ("deleted_at");
CREATE INDEX IF NOT EXISTS "notes_updated_at_idx" ON "{APP_SCHEMA}"."notes" ("updated_at");
CREATE INDEX IF NOT EXISTS "notes_created_by_idx" ON "{APP_SCHEMA}"."notes" ("created_by");"#
        ))
        .await
        .expect("deploy stand-in must create the notes table");

        // Re-establish the per-app role and its grants.
        //
        // The `DROP SCHEMA ... CASCADE` above destroys every GRANT on the
        // schema and its tables along with the schema itself. Recreating the
        // schema does not bring them back, so the data path - which runs
        // `SET LOCAL ROLE app_<id>_role` - was denied with
        // `permission denied for schema default`, and the sanitization rail
        // reported it as a bare `internal error`. That is what made these four
        // tests fail while every test expecting a REFUSAL passed.
        //
        // This runs AFTER the table exists because the grant covers
        // `ALL TABLES IN SCHEMA` at call time.
        //
        // The same hazard is a real one in production, on the restore path:
        // `DROP SCHEMA CASCADE` there destroys the per-app grants AND the
        // schema's `ALTER DEFAULT PRIVILEGES` entries, and `pg_restore
        // --no-privileges` puts none back.
        zeroship_plugin_db::auth::bootstrap::ensure_per_app_role(&pool, APP_SCHEMA)
            .await
            .expect("per-app role + grants must be re-established after the CASCADE");

        drain_open_connections().await;
    });
}

fn count_notes(url: &str) -> i64 {
    let url = url.to_string();
    block_on(async move {
        let (client, connection) = compio_postgres::connect(&url, NoTls).await.unwrap();
        compio::runtime::spawn(async move {
            let _ = connection.run().await;
        })
        .detach();
        let sql = format!("SELECT COUNT(*)::bigint AS c FROM \"{APP_SCHEMA}\".\"notes\"");
        let rows = client.query(&sql, &[]).await.unwrap();
        let count = rows[0].get::<_, i64>("c");
        drop(client);
        drain_open_connections().await;
        count
    })
}

fn create_encrypted_users_table(url: &str, key_id: &str) {
    let url = url.to_string();
    let key_id = key_id.to_string();
    block_on(async move {
        zeroship_plugin_db::set_db_url_for_tests(&url);
        let pool = std::rc::Rc::new(compio_postgres::Pool::connect(&url, 2).await.unwrap());
        pool.batch_execute(&format!(
            r#"CREATE TABLE "{APP_SCHEMA}"."users" (
  id TEXT PRIMARY KEY,
  created_at TIMESTAMPTZ NOT NULL DEFAULT NOW(),
  updated_at TIMESTAMPTZ NOT NULL DEFAULT NOW(),
  created_by TEXT NULL,
  updated_by TEXT NULL,
  version INTEGER NOT NULL DEFAULT 1,
  deleted_at TIMESTAMPTZ NULL,
  email TEXT NOT NULL,
  name TEXT NOT NULL,
  ssn BYTEA NULL
);
CREATE UNIQUE INDEX "users_email_key" ON "{APP_SCHEMA}"."users" (email);
CREATE INDEX "users_deleted_at_idx" ON "{APP_SCHEMA}"."users" (deleted_at);
CREATE INDEX "users_updated_at_idx" ON "{APP_SCHEMA}"."users" (updated_at);
CREATE INDEX "users_created_by_idx" ON "{APP_SCHEMA}"."users" (created_by);
COMMENT ON COLUMN "{APP_SCHEMA}"."users"."ssn" IS 'zsenc:randomised:{key_id}:string';"#
        ))
        .await
        .expect("deploy stand-in must create encrypted users");
        zeroship_plugin_db::auth::bootstrap::ensure_per_app_role(&pool, APP_SCHEMA)
            .await
            .expect("per-app role must receive grants on encrypted users");
        pool.close().await;
        drop(pool);
        drain_open_connections().await;
    });
}

fn user_email_versions(url: &str) -> Vec<(String, i32)> {
    let url = url.to_string();
    block_on(async move {
        let (client, connection) = compio_postgres::connect(&url, NoTls).await.unwrap();
        compio::runtime::spawn(async move {
            let _ = connection.run().await;
        })
        .detach();
        let rows = client
            .query(
                &format!(
                    "SELECT email, version FROM \"{APP_SCHEMA}\".users \
                     WHERE name = 'Red Team' ORDER BY id"
                ),
                &[],
            )
            .await
            .expect("inspect encrypted updateMany rows");
        let values = rows
            .iter()
            .map(|row| (row.get::<_, String>("email"), row.get::<_, i32>("version")))
            .collect();
        drop(client);
        drain_open_connections().await;
        values
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

fn dispatch_zs_for_app(
    url: &str,
    source: &str,
    name: &str,
    app_id: Option<&str>,
) -> (u16, serde_json::Value) {
    init_v8();
    let modules = vec![ModuleEntry {
        specifier: "index.js".into(),
        source: source.into(),
    }];
    let plugins: Vec<Arc<dyn NativePlugin>> = vec![
        DbService::new(DbServiceConfig {
            url: url.to_string(),
            worker_id: "native-transaction-test-worker".to_string(),
            meter: None,
        })
        .expect("db service")
        .plugin(),
    ];
    let mut env_vars = std::collections::HashMap::new();
    if let Some(app_id) = app_id {
        env_vars.insert("APP_ID".to_string(), app_id.to_string());
    }
    let runtime = Runtime::builder()
        .modules(modules)
        .env_vars(env_vars)
        .plugins(plugins)
        .build();
    if app_id.is_some() {
        // Production schema bootstrap initializes this backend handle without
        // applying app migrations. Transactions read the handle directly.
        zeroship_plugin_db::set_db_url_for_tests(url);
        block_on(async {
            zeroship_plugin_db::init_pool_async()
                .await
                .expect("initialize the production Postgres backend");
        });
    }
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
        FetchOutcome::Stream {
            status,
            body_reader,
            ..
        } => block_on(async {
            runtime.start_pump();
            let mut body = Vec::new();
            loop {
                while let Some(chunk) = body_reader.pop() {
                    body.extend_from_slice(&chunk);
                }
                if body_reader.is_done() {
                    break;
                }
                body_reader.wait_for_data().await;
            }
            (status, body)
        }),
        FetchOutcome::Pending { rx, cancel: _ } => {
            block_on(async {
                runtime.start_pump();
                let settled = compio::time::timeout(Duration::from_secs(15), rx.recv())
                    .await
                    .expect("pending timeout")
                    .expect("settled error");
                match settled {
                    SettledFetch::Response { status, body, .. } => (status, body),
                    SettledFetch::Stream {
                        status,
                        body_reader,
                        ..
                    } => {
                        let mut body = Vec::new();
                        loop {
                            while let Some(chunk) = body_reader.pop() {
                                body.extend_from_slice(&chunk);
                            }
                            if body_reader.is_done() {
                                break;
                            }
                            body_reader.wait_for_data().await;
                        }
                        (status, body)
                    }
                    SettledFetch::WebSocketUpgrade { .. } => {
                        panic!("unexpected WebSocketUpgrade variant")
                    }
                }
            })
        }
        FetchOutcome::WebSocketUpgrade { .. } => panic!("unexpected WebSocketUpgrade variant"),
    };
    let body = String::from_utf8_lossy(&body).into_owned();
    let json: serde_json::Value =
        serde_json::from_str(&body).unwrap_or(serde_json::Value::String(body.clone()));
    (status, json)
}

fn dispatch_zs(url: &str, source: &str, name: &str) -> (u16, serde_json::Value) {
    dispatch_zs_for_app(url, source, name, None)
}

/// `_procedures` declaring `setup` (registerModel) plus the per-test
/// handlers `body`.
///
/// `registerModel` lives off `env.db`, on the `__platform`
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

fn build_encrypted_users_src(key_id: &str, body: &str) -> String {
    format!(
        r#"
import {{ env }} from "zeroship";

const __plat = (typeof globalThis.__zsDbPlatform === "function")
    ? globalThis.__zsDbPlatform(env.db)
    : undefined;

function setup(_input, _ctx) {{
    return __plat.registerModel("users", {{
        email: {{ type: "string", required: true, unique: true }},
        name: {{ type: "string", required: true }},
        ssn: {{
            type: "string",
            encrypted: {{ mode: "randomised", keyId: "{key_id}", wraps: "string" }}
        }}
    }});
}}
setup.config = {{ kind: "action" }};

{body}
"#,
    ) + SHIM
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[test]
fn unmigrated_app_autocommit_response_names_migrate() {
    let url = require_pg();
    let app_id = uuid::Uuid::new_v4().simple().to_string();
    let src = build_src(
        r#"
function autocommitBeforeMigrate(_input, _ctx) {
    return env.db.collection("notes").find({}, {});
}
autocommitBeforeMigrate.config = { kind: "action" };
const _procedures = { autocommitBeforeMigrate };
"#,
    );

    let (status, body) = dispatch_zs_for_app(
        &url,
        &src,
        "autocommitBeforeMigrate",
        Some(&app_id),
    );
    assert_eq!(status, 500, "missing role must be a 500 response; body={body}");
    assert_eq!(
        body.get("code").and_then(|v| v.as_str()),
        Some("schema_not_provisioned"),
        "autocommit must preserve the provisioning classification; body={body}"
    );
    assert!(
        body.get("message")
            .and_then(|v| v.as_str())
            .is_some_and(|message| message.contains("zeroship migrate")),
        "autocommit response must name the creator remediation; body={body}"
    );
}

#[test]
fn unmigrated_app_transaction_response_names_migrate() {
    let url = require_pg();
    let app_id = uuid::Uuid::new_v4().simple().to_string();
    let src = build_src(
        r#"
async function transactionBeforeMigrate(_input, _ctx) {
    return await env.db.transaction(async () => "unreachable");
}
transactionBeforeMigrate.config = { kind: "action" };
const _procedures = { transactionBeforeMigrate };
"#,
    );

    let (status, body) = dispatch_zs_for_app(
        &url,
        &src,
        "transactionBeforeMigrate",
        Some(&app_id),
    );
    assert_eq!(status, 500, "missing role must be a 500 response; body={body}");
    assert_eq!(
        body.get("code").and_then(|v| v.as_str()),
        Some("schema_not_provisioned"),
        "transaction setup must preserve the provisioning classification; body={body}"
    );
    assert!(
        body.get("message")
            .and_then(|v| v.as_str())
            .is_some_and(|message| message.contains("zeroship migrate")),
        "transaction response must name the creator remediation; body={body}"
    );
}

#[test]
fn unmigrated_app_streaming_response_names_migrate() {
    let url = require_pg();
    let app_id = uuid::Uuid::new_v4().simple().to_string();
    let src = [
        r#"import { env } from "zeroship";"#,
        include_str!("../../../sdks/bootstrap/dist/fetch-handler.js"),
        r#"
globalThis.__zsDispatch = async (rpc, name, input, ctx) => rpc[name](input, ctx);

async function* streamBeforeMigrate(_input, _ctx) {
    await env.db.collection("notes").find({}, {});
    yield "unreachable";
}
streamBeforeMigrate.config = { kind: "stream" };
const _procedures = { streamBeforeMigrate };
const _fetch = createFetchHandler(async () => ({
    userDefault: {},
    fetch: undefined,
    rpc: _procedures,
}));
export default { fetch: _fetch, rpc: _procedures };
"#,
    ]
    .join("\n");

    let (status, body) =
        dispatch_zs_for_app(&url, &src, "streamBeforeMigrate", Some(&app_id));
    assert_eq!(status, 200, "SSE errors stay inside a 200 stream; body={body}");
    let text = body.as_str().expect("SSE body must be text");
    assert!(
        text.contains("schema_not_provisioned")
            || text.contains("SCHEMA_NOT_PROVISIONED"),
        "streaming response must preserve the provisioning code; body={text}"
    );
    assert!(
        text.contains("zeroship migrate"),
        "streaming response must name the creator remediation; body={text}"
    );
    assert!(
        !text.contains("internal error"),
        "streaming response must not replace the remediation; body={text}"
    );
}

/// Commit on resolve: the callback inserts a row and resolves; the row
/// persists and is visible after the transaction commits.
#[test]
fn transaction_commits_on_resolve() {
    let url = require_pg();
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
    let url = require_pg();
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
    let url = require_pg();
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
    let url = require_pg();
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

/// A write rolled back to a SAVEPOINT must not publish its change event
/// when the OUTER transaction commits.
///
/// `pending_emits` is a flat, app-keyed `Vec<ChangeEvent>`
/// (`context.rs:231`) with no savepoint scoping, and the nested settle arm
/// pops the savepoint depth and runs `ROLLBACK TO SAVEPOINT` without
/// touching that buffer (`transaction/mod.rs:989-999`). The top-level
/// commit then drains *everything* queued for the app
/// (`drain_pending_emits_on_commit`, `exec.rs:600-612`, called at
/// `transaction/mod.rs:1056`). So a subscriber is told about a row that
/// was rolled back and does not exist.
///
/// The live subscription is load-bearing, not scaffolding: `emit_for_rows`
/// returns early unless `broker::has_subscribers(app, collection)`
/// (`exec.rs:501-504`), so without it nothing is ever queued and this test
/// would pass vacuously against the very defect it exists to catch.
#[test]
fn savepoint_rollback_must_not_publish_its_change_event_at_outer_commit() {
    let url = require_pg();
    reset_schema(&url);

    // Same thread as `block_on`'s runtime (`RT.with`), so this shares the
    // thread-local broker the dispatch path publishes into.
    let sub = zeroship_plugin_db::broker::subscribe(APP_SCHEMA, "notes");

    let src = build_src(
        r#"
async function savepointEmitLeak(_input, _ctx) {
    const r = await env.db.transaction(async (tx) => {
        try {
            await env.db.transaction(async (tx2) => {
                await tx2.notes.insert({ title: "inner-doomed" });
                throw new Error("inner abort");
            });
        } catch (e) {
            // swallow - the outer continues and commits
        }
        await tx.notes.insert({ title: "outer-survives" });
        return "outer-committed";
    });
    return { txResult: r };
}
savepointEmitLeak.config = { kind: "action" };
const _procedures = { setup, savepointEmitLeak };
"#,
    );

    let (status, _b) = dispatch_zs(&url, &src, "setup");
    assert_eq!(status, 200);

    let (status, body) = dispatch_zs(&url, &src, "savepointEmitLeak");
    assert_eq!(status, 200, "handler should succeed: {body}");

    // Ground truth: exactly one row survived.
    assert_eq!(
        count_notes(&url),
        1,
        "precondition: only the outer row persists; body={body}"
    );

    let mut published: Vec<String> = Vec::new();
    while let Some(msg) = sub.pop() {
        if let zeroship_plugin_db::broker::SubscriptionMessage::Change(ev) = msg {
            if let Some(title) = ev.new_tuple.get("title") {
                published.push(title.clone());
            }
        }
    }

    assert!(
        !published.iter().any(|t| t == "inner-doomed"),
        "a row rolled back to its SAVEPOINT must never be published at the \
         outer COMMIT, but the subscriber received it; published={published:?}"
    );
}

/// Nested transaction inner resolve releases the savepoint: both the
/// inner and outer writes commit (RELEASE SAVEPOINT then top-level
/// COMMIT).
#[test]
fn nested_inner_resolve_releases_savepoint() {
    let url = require_pg();
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
    let url = require_pg();
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
    let url = require_pg();
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
    let url = require_pg();
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

#[test]
fn update_many_randomised_failure_is_atomic_postgres() {
    let url = require_pg();
    reset_schema(&url);
    let key_id = "update_many_atomic_pg";
    create_encrypted_users_table(&url, key_id);
    let _keys = zeroship_plugin_db::supply_root_keys_for_tests(&[(key_id, &"b".repeat(64))]);

    let src = build_encrypted_users_src(
        key_id,
        r#"
async function seed(_input, _ctx) {
    const coll = env.db.collection("users");
    try {
        await coll.insert({
            id: "user_a",
            email: "alice@example.com",
            name: "Red Team",
            ssn: "123-45-6789"
        });
        await coll.insert({
            id: "user_b",
            email: "bob@example.com",
            name: "Red Team",
            ssn: "222-33-4444"
        });
        return { failure: null };
    } catch (err) {
        return {
            failure: {
                code: err?.code ?? null,
                message: err?.message ?? String(err),
                hint: err?.hint ?? null,
            }
        };
    }
}
seed.config = { kind: "action" };

async function failBulk(_input, _ctx) {
    const coll = env.db.collection("users");
    let failure = null;
    try {
        await coll.updateMany(
            { name: "Red Team" },
            { email: "bulk-collision@example.com", ssn: "999-88-7777" },
        );
    } catch (err) {
        failure = { code: err?.code ?? null, message: err?.message ?? String(err) };
    }
    const after = await coll.find({ name: "Red Team" });
    return { failure, after };
}
failBulk.config = { kind: "action" };

const _procedures = { setup, seed, failBulk };
"#,
    );

    let (status, body) = dispatch_zs(&url, &src, "setup");
    assert_eq!(status, 200, "setup failed: {body}");
    let (status, body) = dispatch_zs(&url, &src, "seed");
    assert_eq!(status, 200, "seed failed: {body}");
    assert!(body["json"]["failure"].is_null(), "seed failed: {body}");

    zeroship_plugin_db::crud::reset_write_path_counters_for_tests();
    let (status, body) = dispatch_zs(&url, &src, "failBulk");
    assert_eq!(status, 200, "caught updateMany failure must remain inspectable: {body}");
    let result = &body["json"];
    assert_eq!(result["failure"]["code"], "unique_violation", "body={body}");
    let after = result["after"]
        .as_array()
        .expect("caller-visible post-failure rows must be an array");
    assert_eq!(after.len(), 2, "the exercised target set must be non-empty: {body}");
    let counters = zeroship_plugin_db::crud::write_path_counters_for_tests();
    assert_eq!(
        counters.target_row_resolution_calls, 1,
        "the PG failure must occur on the per-row fan-out path: {counters:?}"
    );
    assert_eq!(
        counters.target_row_resolution_sql.len(),
        1,
        "the PG fan-out must execute one non-empty target probe: {counters:?}"
    );
    let expected_probe_suffix = format!(
        " LIMIT {} FOR UPDATE",
        zeroship_plugin_db::query::MAX_QUERY_LIMIT + 1
    );
    assert!(
        counters.target_row_resolution_sql[0].ends_with(&expected_probe_suffix),
        "the PG target probe must cap and lock the rows it will update: {counters:?}"
    );
    let mut caller_visible: Vec<(String, i64)> = after
        .iter()
        .map(|row| {
            (
                row["email"].as_str().expect("email string").to_string(),
                row["version"].as_i64().expect("version integer"),
            )
        })
        .collect();
    caller_visible.sort();
    assert_eq!(
        caller_visible,
        vec![
            ("alice@example.com".to_string(), 1),
            ("bob@example.com".to_string(), 1),
        ]
    );
    assert_eq!(
        user_email_versions(&url),
        vec![
            ("alice@example.com".to_string(), 1),
            ("bob@example.com".to_string(), 1),
        ],
        "PostgreSQL must roll back the successful prefix before reporting failure"
    );
}

/// L8 REVEAL: a transaction PostgreSQL rolled back must not be reported as a
/// successful commit.
///
/// PostgreSQL answers `COMMIT` with a `ROLLBACK` command tag when the
/// transaction is in a failed state. MEASURED directly against the server:
///
/// ```text
/// BEGIN; SELECT 1/0;  -> ERROR: division by zero
/// COMMIT;             -> ROLLBACK          (control: a clean tx replies COMMIT)
/// ```
///
/// The driver detects exactly this and turns it into an error
/// (`libs/compio-postgres/src/transaction.rs:186-188`), but plugin-db's raw
/// executor throws the command tag away - `client_exec` returns
/// `Ok(rows.len() as u64)`
/// (`crates/zeroship-plugin-db/src/backend/postgres.rs:201-212`) - and the
/// explicit-transaction settle path sends its `COMMIT` through that same
/// function (`crates/zeroship-plugin-db/src/transaction/mod.rs:1001`).
///
/// SCOPE: this drives an EXPLICIT creator transaction on purpose. The autocommit
/// path already goes through the driver's own `tx.commit()` wrapper
/// (`crates/zeroship-plugin-db/src/exec.rs:344`), which checks the tag, so the
/// same test written against autocommit passes pre-fix and proves nothing.
///
/// The callback swallows a genuine DB-level error so it RESOLVES: the
/// orchestrator then proceeds to COMMIT a poisoned transaction, which is the
/// state that reproduces the defect.
#[test]
fn commit_that_postgres_rolled_back_must_not_report_success_l8() {
    let url = require_pg();
    reset_schema(&url);

    let src = build_src(
        r#"
async function poisonThenCommit(_input, _ctx) {
    const r = await env.db.transaction(async (tx) => {
        await tx.notes.insert({ id: "l8-keep", title: "should be durable" });
        try {
            // Duplicate primary key: a real server-side error, which puts the
            // transaction into the failed state where COMMIT answers ROLLBACK.
            await tx.notes.insert({ id: "l8-keep", title: "duplicate" });
        } catch (_e) {
            // Swallowed on purpose so the callback resolves.
        }
        return "ok";
    });
    return { txResult: r };
}
poisonThenCommit.config = { kind: "action" };
const _procedures = { setup, poisonThenCommit };
"#,
    );

    let (status, body) = dispatch_zs(&url, &src, "setup");
    assert_eq!(status, 200, "setup failed: {body}");

    let (status, body) = dispatch_zs(&url, &src, "poisonThenCommit");
    let rows = count_notes(&url);

    // The server rolled the transaction back, so the row is gone. The contract
    // that must hold is simply: SUCCESS MEANS DURABLE. If the dispatch reported
    // success, the write it claimed to commit has to be there.
    if status == 200 {
        assert_eq!(
            rows, 1,
            "commit reported SUCCESS (status 200, body={body}) but PostgreSQL \
             rolled the transaction back and the row is gone (count={rows}). \
             A rolled-back commit must not be reported as committed."
        );
    } else {
        assert_eq!(rows, 0, "a failed commit must leave nothing behind; body={body}");
    }
}
