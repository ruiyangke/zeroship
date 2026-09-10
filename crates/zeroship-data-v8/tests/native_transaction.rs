//! End-to-end tests for the native `Db.transaction(fn)`
//! orchestrator against real Postgres.
//!
//! These dispatch through a real `Runtime` + the `DbPlugin` and call
//! `env.db.transaction(async tx => {...})`. They exercise the full
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
//! **`env.db.transaction` HERE IS THE BOOTSTRAP WRAPPER, NOT THE RAW
//! v8_method, and this header claimed the opposite until 2026-09-04.** Every
//! dispatch in this file goes through `dispatch_zs_for_app_with_descriptor`,
//! which sets `.runtime_descriptor(Some(...))`; the runtime then splices
//! `runtime-entry.js`, which calls `installSchema`, which overwrites
//! `env.db.transaction` with its own `transactionImpl`
//! (`sdks/bootstrap/src/install-schema.ts`). That wrapper is what makes
//! `tx.notes` exist, so it is not removable - it is the shape a deployed app
//! actually calls. Its published contract is
//! `transaction(fn) -> Promise<Result<R>>` **which never throws**
//! (`sdks/db/src/db-types.ts`, `docs/reference/db.md`): a rollback, a setup
//! denial or a depth-cap refusal arrives as `result.error`, not as an
//! exception. Assertions in this file must read that envelope, or rethrow
//! `result.error` deliberately when the test is about the terminal HTTP
//! remedy. Five tests asserted a throw against this wrapper and had failed
//! continuously since it was installed; the false sentence above is why they
//! were repeatedly waved through as "pre-existing".
//!
//! Requires: the test PostgreSQL named by the overlay
//! (`deploy/ops/zeroship.test.toml`, written by
//! `tests/provision_test_backends.sh`) or by `PG_TEST_URL`. There is no
//! compiled default; see `crates/zeroship-core/src/config/test_overlay.rs`.
//! Run: `cargo test -p zeroship-data-v8 --features test-helpers --test test_helpers \
//!        -- --test-threads=1 native_transaction::`
//!
//! Each runtime receives the same descriptor shape that a deploy carries; the
//! tables are applied ahead of boot by the fixture. The orchestrator logic is
//! also covered without a DB by the Rust unit tests + tx-view shape
//! tests in `crates/zeroship-data-orm/src/transaction/mod.rs` and
//! `crates/zeroship-data-v8/src/v8_classes/transaction.rs`,
//! the `db_v8_class.rs` surface tests, the SQLite SAVEPOINT SQL tests in
//! `sqlite_integration.rs`, and the SDK-side mock tests in
//! `sdks/db/tests/p9-pr3-native-transaction.test.ts`.

// `support` is declared once by `tests/test_helpers.rs`, the entry file this
// module hangs off; its header says why a second declaration here would be a
// second copy of the `Once` guarding the residue sweep.
use crate::support;

use std::sync::Arc;
use std::time::Duration;

use compio_postgres::NoTls;
use zeroship_data_v8::service::{DbService, DbServiceConfig};
use zeroship_runtime::channel::CancelFlag;
use zeroship_runtime::plugin::NativePlugin;
use zeroship_runtime::runtime::Runtime;
use zeroship_runtime::{EnvSnapshot, FetchOutcome, ModuleEntry, RequestCtx, SettledFetch, init_v8};

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
/// PostgreSQL is required by ordinary package tests. An unavailable server
/// fails the test instead of reporting success without exercising a transaction.
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
    // Every test enters here before it touches the database, and
    // `Once::call_once` blocks the rest until the first returns, so this is the
    // barrier that makes an unbounded residue sweep safe.
    support::sweep_prior_run_residue_once(&url);
    url
}

// `APP_SCHEMA = "default"` lived here and is DELETED. Eleven of the fifteen
// tests below reached for that one schema through `reset_schema`, which opens
// with `DROP SCHEMA ... CASCADE`, so at the default thread count they raced to
// destroy each other's `notes` table: measured 2026-09-04, 13 passed / 11 failed
// parallel against 19 / 5 serial, with
// `duplicate key value violates unique constraint "pg_namespace_nspname_index",
// Key (nspname)=(default)` in the log.
//
// THE "19 / 5 SERIAL" HALF OF THAT IS NOT REPRODUCIBLE, and the correction is
// worth more than the number. Re-measured 2026-09-04 at the pre-parallelism
// commit, serially, this file scores 24 passed / 0 failed on a freshly created
// database AND on the long-lived shared one - so those 5 were never a property
// of the code, and nothing here "fixed" them. They were residue: objects an
// earlier run left in whichever database the original measurement reused. Read
// any count from this suite as a statement about a (code, database) pair, and
// name the database when you record one.
//
// The name was also INHERITED rather than chosen: these tests booted with
// `EnvSnapshot::empty()`, so the runtime's no-`APP_ID` fallback
// (`crates/zeroship-runtime/src/core/plugin.rs`) picked `default` for them.
// Each test now mints its own id with `test_app_id!()` and injects it, which
// isolates the schema, the per-app role and the broker key at once - and stops
// the fixture depending on a fallback continuing to exist.

/// Drop the app schema, then PROVISION the `notes` table the way the engine /
/// deploy-apply does.
///
/// `zeroship-migrate` is the sole PostgreSQL schema authority and creates the
/// schema at deploy time, before the app serves. The table must already exist
/// when the handlers run, so this fixture stands in for deploy-time apply.
/// Release this runtime's connections before it falls out of scope.
///
/// Every helper here builds its own runtime and drops it when the block ends.
/// Closing a connection is asynchronous, so a runtime that stops first leaves
/// the socket - and the server-side backend - alive for the life of the test
/// process. Draining inside the block keeps the runtime alive long enough to
/// finish the close.
async fn drain_open_connections() {
    zeroship_data_v8::reset_context_for_tests();
    if !compio_postgres::drain_connections(std::time::Duration::from_secs(2)).await {
        eprintln!(
            "DRAIN-TIMEOUT: {} connection(s) still live",
            compio_postgres::live_connections()
        );
    }
}

fn reset_schema(url: &str, app: &str) {
    let url = url.to_string();
    let app = app.to_string();
    block_on(async move {
        let (client, connection) = compio_postgres::connect(&url, NoTls).await.unwrap();
        compio::runtime::spawn(async move {
            let _ = connection.run().await;
        })
        .detach();
        client
            .execute(&format!("DROP SCHEMA IF EXISTS \"{app}\" CASCADE"), &[])
            .await
            .unwrap();
        drop(client);

        // Deploy stand-in, as RAW SQL. `zeroship-migrate` is the PG schema
        // authority, so a test that needs the `notes` table creates it. The
        // per-thread context still needs the URL
        // for the transaction orchestrator under test.
        zeroship_data_v8::set_db_url_for_tests(&url);
        let pool = std::rc::Rc::new(compio_postgres::Pool::connect(&url, 2).await.unwrap());
        pool.batch_execute(&format!(
            r#"CREATE SCHEMA IF NOT EXISTS "{app}";
CREATE TABLE "{app}"."notes" (
  id TEXT PRIMARY KEY,
  created_at TIMESTAMPTZ NOT NULL DEFAULT NOW(),
  updated_at TIMESTAMPTZ NOT NULL DEFAULT NOW(),
  created_by TEXT NULL,
  updated_by TEXT NULL,
  version INTEGER NOT NULL DEFAULT 1,
  deleted_at TIMESTAMPTZ NULL,
  "title" TEXT NOT NULL
);
CREATE INDEX IF NOT EXISTS "notes_deleted_at_idx" ON "{app}"."notes" ("deleted_at");
CREATE INDEX IF NOT EXISTS "notes_updated_at_idx" ON "{app}"."notes" ("updated_at");
CREATE INDEX IF NOT EXISTS "notes_created_by_idx" ON "{app}"."notes" ("created_by");"#
        ))
        .await
        .expect("deploy stand-in must create the notes table");

        // Re-establish the per-app role, then the binding's column grants.
        //
        // The `DROP SCHEMA ... CASCADE` above destroys every GRANT on the
        // schema and its tables along with the schema itself. Recreating the
        // schema does not bring them back, so the data path - which runs
        // `SET LOCAL ROLE app_<id>_role` - was denied with
        // `permission denied for schema default`, and the sanitization rail
        // reported it as a bare `internal error`. That is what made these four
        // tests fail while every test expecting a REFUSAL passed.
        //
        // The same hazard is a real one in production, on the restore path:
        // `DROP SCHEMA CASCADE` there destroys the per-app grants AND the
        // schema's `ALTER DEFAULT PRIVILEGES` entries, and `pg_restore
        // --no-privileges` puts none back.
        zeroship_data_orm::auth::bootstrap::ensure_per_app_role(&pool, &app)
            .await
            .expect("per-app role must be re-established after the CASCADE");
        support::grant_all_runtime_table_columns(&pool, &app, "notes").await;

        drain_open_connections().await;
    });
}

fn count_notes(url: &str, app: &str) -> i64 {
    let url = url.to_string();
    let app = app.to_string();
    block_on(async move {
        let (client, connection) = compio_postgres::connect(&url, NoTls).await.unwrap();
        compio::runtime::spawn(async move {
            let _ = connection.run().await;
        })
        .detach();
        let sql = format!("SELECT COUNT(*)::bigint AS c FROM \"{app}\".\"notes\"");
        let rows = client.query(&sql, &[]).await.unwrap();
        let count = rows[0].get::<_, i64>("c");
        drop(client);
        drain_open_connections().await;
        count
    })
}

/// Run one statement against the app schema as the owner.
///
/// For tests that need a constraint `reset_schema` does not create, because the
/// constraint IS the fixture. See
/// [`commit_that_postgres_rolled_back_must_not_report_success_l8`], which has to
/// poison a transaction with a server-side error the platform cannot absorb.
fn exec_owner_sql(url: &str, sql: &str) {
    let url = url.to_string();
    let sql = sql.to_string();
    block_on(async move {
        let (client, connection) = compio_postgres::connect(&url, NoTls).await.unwrap();
        compio::runtime::spawn(async move {
            let _ = connection.run().await;
        })
        .detach();
        client.batch_execute(&sql).await.unwrap();
        drop(client);
        drain_open_connections().await;
    });
}

/// The table the deploy-time migration would have created for
/// [`build_encrypted_users_src`]'s declared schema.
///
/// It used to end with `COMMENT ON COLUMN ... 'zero-migrate:enc:randomised:<key>:string'`.
/// That sentinel is gone with the catalog read that recovered it: the encryption
/// metadata the CRUD passes act on now comes from the RUNTIME DESCRIPTOR, which
/// in these tests is installed from the RuntimeBuilder descriptor. `key_id` is
/// still a parameter because the descriptor and supplied root key have to
/// agree on it; nothing in the SQL below reads it any more.
fn create_encrypted_users_table(url: &str, app: &str, key_id: &str) {
    let url = url.to_string();
    let app = app.to_string();
    let _ = key_id;
    block_on(async move {
        zeroship_data_v8::set_db_url_for_tests(&url);
        let pool = std::rc::Rc::new(compio_postgres::Pool::connect(&url, 2).await.unwrap());
        pool.batch_execute(&format!(
            r#"CREATE TABLE "{app}"."users" (
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
CREATE UNIQUE INDEX "users_email_key" ON "{app}"."users" (email);
CREATE INDEX "users_deleted_at_idx" ON "{app}"."users" (deleted_at);
CREATE INDEX "users_updated_at_idx" ON "{app}"."users" (updated_at);
CREATE INDEX "users_created_by_idx" ON "{app}"."users" (created_by);"#
        ))
        .await
        .expect("deploy stand-in must create encrypted users");
        zeroship_data_orm::auth::bootstrap::ensure_per_app_role(&pool, &app)
            .await
            .expect("per-app role must exist for encrypted users");
        support::grant_all_runtime_table_columns(&pool, &app, "users").await;
        pool.close().await;
        drop(pool);
        drain_open_connections().await;
    });
}

fn user_email_versions(url: &str, app: &str) -> Vec<(String, i32)> {
    let url = url.to_string();
    let app = app.to_string();
    block_on(async move {
        let (client, connection) = compio_postgres::connect(&url, NoTls).await.unwrap();
        compio::runtime::spawn(async move {
            let _ = connection.run().await;
        })
        .detach();
        let rows = client
            .query(
                &format!(
                    "SELECT email, version FROM \"{app}\".users \
                     WHERE name = 'Red Team' ORDER BY email"
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

fn notes_runtime_descriptor() -> String {
    serde_json::json!({
        "version": 2,
        "collections": {
            "notes": {
                "fields": {
                    "title": {"type": "string", "required": true},
                },
                "options": {
                    "softDelete": false,
                    "versioning": false,
                    "strictness": "strict",
                },
                "indexes": [],
            },
        },
    })
    .to_string()
}

fn users_runtime_descriptor(key_id: &str) -> String {
    serde_json::json!({
        "version": 2,
        "collections": {
            "users": {
                "fields": {
                    "email": {"type": "string", "required": true, "unique": true},
                    "name": {"type": "string", "required": true},
                    "ssn": {
                        "type": "string",
                        "encrypted": {
                            "mode": "randomised",
                            "keyId": key_id,
                            "wraps": "string",
                        },
                    },
                },
                "options": {
                    "softDelete": false,
                    "versioning": false,
                    "strictness": "strict",
                },
                "indexes": [],
            },
        },
    })
    .to_string()
}

fn dispatch_zs_for_app_with_descriptor(
    url: &str,
    source: &str,
    name: &str,
    app_id: Option<&str>,
    descriptor: String,
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
        .runtime_descriptor(Some(descriptor))
        .build();
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
        FetchOutcome::Pending { rx, cancel: _ } => block_on(async {
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
        }),
        FetchOutcome::WebSocketUpgrade { .. } => panic!("unexpected WebSocketUpgrade variant"),
    };
    let body = String::from_utf8_lossy(&body).into_owned();
    let json: serde_json::Value =
        serde_json::from_str(&body).unwrap_or(serde_json::Value::String(body.clone()));
    (status, json)
}

fn dispatch_zs_for_app(
    url: &str,
    source: &str,
    name: &str,
    app_id: Option<&str>,
) -> (u16, serde_json::Value) {
    dispatch_zs_for_app_with_descriptor(url, source, name, app_id, notes_runtime_descriptor())
}

fn dispatch_zs(url: &str, source: &str, name: &str, app: &str) -> (u16, serde_json::Value) {
    dispatch_zs_for_app(url, source, name, Some(app))
}

fn dispatch_zs_with_descriptor(
    url: &str,
    source: &str,
    name: &str,
    app: &str,
    descriptor: String,
) -> (u16, serde_json::Value) {
    dispatch_zs_for_app_with_descriptor(url, source, name, Some(app), descriptor)
}

/// Build a module around the per-test handlers in `body`.
fn build_src(body: &str) -> String {
    format!(
        r#"
import {{ env }} from "zeroship";

{body}
"#
    ) + SHIM
}

fn build_encrypted_users_src(body: &str) -> String {
    format!(
        r#"
import {{ env }} from "zeroship";

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

    let (status, body) = dispatch_zs_for_app(&url, &src, "autocommitBeforeMigrate", Some(&app_id));
    assert_eq!(
        status, 500,
        "missing role must be a 500 response; body={body}"
    );
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

/// The unmigrated-app classification must reach the creator, and it must reach
/// them with the terminal HTTP remedy when nothing catches it.
///
/// `env.db.transaction` is the bootstrap wrapper (module header), so it folds
/// the classified denial into `result.error` and the handler answers 200 with
/// `{data:null,error:{...}}`. THAT IS THE PUBLISHED CONTRACT, not a defect -
/// but it means the handler has to rethrow to make the response terminal, and
/// this test is about the terminal response. `Error#message` is non-enumerable,
/// so a bare `JSON.stringify(result.error)` would drop the remediation text;
/// rethrowing hands the error to the shim, which copies `message` out
/// explicitly and reads `status` off the error object.
#[test]
fn unmigrated_app_transaction_response_names_migrate() {
    let url = require_pg();
    let app_id = uuid::Uuid::new_v4().simple().to_string();
    let src = build_src(
        r#"
async function transactionBeforeMigrate(_input, _ctx) {
    const r = await env.db.transaction(async () => "unreachable");
    if (r.error) throw r.error;
    return r.data;
}
transactionBeforeMigrate.config = { kind: "action" };
const _procedures = { transactionBeforeMigrate };
"#,
    );

    let (status, body) = dispatch_zs_for_app(&url, &src, "transactionBeforeMigrate", Some(&app_id));
    assert_eq!(
        status, 500,
        "missing role must be a 500 response; body={body}"
    );
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

/// A classified failure from the top-level transaction's session setup must
/// survive the begin-completion event. Revoking the login role's membership in
/// the app role makes the first setup statement, `SET LOCAL ROLE`, return the
/// measured SQLSTATE 42501. The callback must never run, and the error app code
/// sees must name the terminal grant denial rather than generic BEGIN.
///
/// Two arms, one variable apart. The first reads `result.error` off the
/// wrapper's envelope, which is the published contract and answers 200. The
/// second RETHROWS it, which is the only way the terminal HTTP remedy reaches
/// the wire - and that 403 arm had never executed before 2026-09-04, because
/// the first arm's `try/catch` shape panicked the test first.
#[test]
fn revoked_grant_transaction_surfaces_grant_revoked() {
    let admin_url = require_pg();
    let suffix = uuid::Uuid::new_v4().simple().to_string();
    let app_id = format!("zs_txgrant_{suffix}");
    let login = format!("zs_txlogin_{}", &suffix[..16]);
    let password = "ZsTxGrant9";
    let app_role = zeroship_core::database_role::per_app_role_name(&app_id)
        .expect("grant-revocation app id must produce a valid PostgreSQL role name");
    let (scheme, address) = admin_url
        .split_once("://")
        .and_then(|(scheme, rest)| rest.rsplit_once('@').map(|(_, address)| (scheme, address)))
        .expect("PG_TEST_URL must contain scheme and login credentials");
    let worker_url = format!("{scheme}://{login}:{password}@{address}");

    block_on(async {
        let (admin, connection) = compio_postgres::connect(&admin_url, NoTls)
            .await
            .expect("connect grant-revocation admin");
        compio::runtime::spawn(async move {
            let _ = connection.run().await;
        })
        .detach();

        admin
            .batch_execute(&format!(
                "CREATE ROLE \"{login}\" LOGIN PASSWORD '{password}' \
                   NOSUPERUSER NOCREATEDB NOCREATEROLE NOREPLICATION NOINHERIT; \
                 CREATE ROLE \"{app_role}\" NOLOGIN NOSUPERUSER NOCREATEDB \
                   NOCREATEROLE NOREPLICATION; \
                 CREATE SCHEMA \"{app_id}\"; \
                 GRANT \"{app_role}\" TO \"{login}\""
            ))
            .await
            .expect("provision login and app role");

        // Prove this login can set the role before the one variable under test
        // changes. Keeping this same backend alive also proves PostgreSQL
        // observes the membership revoke without reconnecting it.
        let (worker, worker_connection) = compio_postgres::connect(&worker_url, NoTls)
            .await
            .expect("connect temporary worker login");
        compio::runtime::spawn(async move {
            let _ = worker_connection.run().await;
        })
        .detach();
        let set_local_role_sql = zeroship_data_orm::auth::bootstrap::set_local_role_sql(&app_id)
            .expect("grant-revocation app id must produce valid SET LOCAL ROLE SQL");
        worker.batch_execute("BEGIN").await.expect("control BEGIN");
        worker
            .batch_execute(&set_local_role_sql)
            .await
            .expect("membership must permit SET LOCAL ROLE before revoke");
        worker
            .batch_execute("ROLLBACK")
            .await
            .expect("control ROLLBACK");

        admin
            .batch_execute(&format!("REVOKE \"{app_role}\" FROM \"{login}\""))
            .await
            .expect("revoke app-role membership");
        worker.batch_execute("BEGIN").await.expect("oracle BEGIN");
        let revoked = worker
            .batch_execute(&set_local_role_sql)
            .await
            .expect_err("revoked membership must deny SET LOCAL ROLE");
        assert_eq!(
            revoked.code().map(compio_postgres::error::SqlState::code),
            Some("42501"),
            "the setup refusal must be PostgreSQL insufficient_privilege"
        );
        worker
            .batch_execute("ROLLBACK")
            .await
            .expect("oracle ROLLBACK");
        drop(worker);
        drop(admin);
        drain_open_connections().await;
    });

    let src = build_src(
        r#"
async function transactionAfterGrantRevoke(_input, _ctx) {
    let callbackReached = false;
    // The wrapper folds the setup denial into `result.error` and never
    // throws, so read the envelope (module header).
    const r = await env.db.transaction(async () => {
        callbackReached = true;
        return "unreachable";
    });
    return { code: r.error?.code ?? null, callbackReached };
}
transactionAfterGrantRevoke.config = { kind: "action" };

async function transactionAfterGrantRevokeUncaught(_input, _ctx) {
    // Rethrow so the denial is TERMINAL: that is what puts the classified
    // 403 on the wire instead of a 200 carrying an ignored `error`.
    const r = await env.db.transaction(async () => "unreachable");
    if (r.error) throw r.error;
    return r.data;
}
transactionAfterGrantRevokeUncaught.config = { kind: "action" };

const _procedures = {
    transactionAfterGrantRevoke,
    transactionAfterGrantRevokeUncaught,
};
"#,
    );
    let (status, body) = dispatch_zs_for_app(
        &worker_url,
        &src,
        "transactionAfterGrantRevoke",
        Some(&app_id),
    );
    let (uncaught_status, uncaught_body) = dispatch_zs_for_app(
        &worker_url,
        &src,
        "transactionAfterGrantRevokeUncaught",
        Some(&app_id),
    );

    // Clean up before asserting the creator-visible result so the intentional
    // red run does not leave roles behind on the shared live-test server.
    block_on(async {
        drain_open_connections().await;
        let (admin, connection) = compio_postgres::connect(&admin_url, NoTls)
            .await
            .expect("reconnect grant-revocation admin for cleanup");
        compio::runtime::spawn(async move {
            let _ = connection.run().await;
        })
        .detach();
        admin
            .batch_execute(&format!(
                "DROP SCHEMA IF EXISTS \"{app_id}\" CASCADE; \
                 DROP ROLE IF EXISTS \"{app_role}\"; \
                 DROP ROLE IF EXISTS \"{login}\""
            ))
            .await
            .expect("clean up grant-revocation fixture");
        drop(admin);
        drain_open_connections().await;
    });

    assert_eq!(
        status, 200,
        "the handler reads the setup denial off the envelope; body={body}"
    );
    let result = body.get("json").expect("caught error result");
    assert_eq!(
        result
            .get("callbackReached")
            .and_then(serde_json::Value::as_bool),
        Some(false),
        "session setup must fail before creator callback execution; body={body}"
    );
    assert_eq!(
        result.get("code").and_then(serde_json::Value::as_str),
        Some("GRANT_REVOKED"),
        "the classified terminal denial must survive transaction BEGIN; body={body}"
    );
    assert_eq!(
        uncaught_status, 403,
        "an uncaught grant denial must carry its terminal HTTP remedy; body={uncaught_body}"
    );
    assert_eq!(
        uncaught_body
            .get("code")
            .and_then(serde_json::Value::as_str),
        Some("GRANT_REVOKED"),
        "the 403 response must retain the classified denial code; body={uncaught_body}"
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

    let (status, body) = dispatch_zs_for_app(&url, &src, "streamBeforeMigrate", Some(&app_id));
    assert_eq!(
        status, 200,
        "SSE errors stay inside a 200 stream; body={body}"
    );
    let text = body.as_str().expect("SSE body must be text");
    assert!(
        text.contains("schema_not_provisioned") || text.contains("SCHEMA_NOT_PROVISIONED"),
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

/// Worker arguments and results preserve binary slices and wide integers.
#[test]
fn native_bytes_and_bigints_round_trip_through_worker_transactions() {
    let url = require_pg();
    let app = crate::test_app_id!();
    let app = app.as_str();
    reset_schema(&url, app);
    let role = zeroship_core::database_role::per_app_role_name(app).unwrap();
    exec_owner_sql(
        &url,
        &format!(
            "ALTER TABLE \"{app}\".notes ADD COLUMN payload BYTEA, ADD COLUMN counter BIGINT; \
         GRANT SELECT, INSERT, UPDATE ON \"{app}\".notes TO \"{role}\""
        ),
    );
    let mut descriptor: serde_json::Value =
        serde_json::from_str(&notes_runtime_descriptor()).unwrap();
    descriptor["collections"]["notes"]["fields"]["payload"] = serde_json::json!({"type":"bytes"});
    descriptor["collections"]["notes"]["fields"]["counter"] = serde_json::json!({"type":"bigInt"});
    let src = build_src(
        r#"
function unwrap(result) { if (result.error) throw result.error; return result.data; }
async function nativeValues() {
    const result = await env.db.transaction(async tx => {
        const inserted = await tx.notes.insert({
            title: "native", payload: new Uint8Array([9, 0, 255, 8]).subarray(1, 3),
            counter: 9223372036854775806n,
        });
        if (!(inserted.payload instanceof Uint8Array)) throw new Error("bytes lost their type");
        if (typeof inserted.counter !== "bigint") throw new Error("integer lost precision");
        const updated = await tx.notes.update({id: inserted.id}, {counter: {$inc: 1n}});
        return {bytes: Array.from(updated.payload), counter: updated.counter.toString()};
    });
    return unwrap(result);
}
nativeValues.config = {kind: "action"};
const _procedures = {nativeValues};
"#,
    );
    let (status, body) =
        dispatch_zs_with_descriptor(&url, &src, "nativeValues", app, descriptor.to_string());
    assert_eq!(status, 200, "native worker values: {body}");
    assert_eq!(
        body["json"],
        serde_json::json!({"bytes":[0,255], "counter":"9223372036854775807"})
    );
    assert_eq!(count_notes(&url, app), 1);
}

#[test]
fn native_json_types_round_trip_through_worker_transactions() {
    let url = require_pg();
    let app = crate::test_app_id!();
    let app = app.as_str();
    reset_schema(&url, app);
    let role = zeroship_core::database_role::per_app_role_name(app).unwrap();
    exec_owner_sql(
        &url,
        &format!(
            "ALTER TABLE \"{app}\".notes ADD COLUMN payload JSONB; \
             GRANT SELECT, INSERT, UPDATE ON \"{app}\".notes TO \"{role}\""
        ),
    );
    let mut descriptor: serde_json::Value =
        serde_json::from_str(&notes_runtime_descriptor()).unwrap();
    descriptor["collections"]["notes"]["fields"]["payload"] =
        serde_json::json!({"type":"json", "nullable":true});
    let source = build_src(
        r#"
async function jsonValues() {
    const values = ["true", "null", "42", "[1]", '{"key":1}', '"nested"',
                    "plain text", true, false, 42, 1.5, null, {key:"true"}, [false,"null"]];
    const result = await env.db.transaction(async tx => {
        for (const payload of values) {
            const inserted = await tx.notes.insert({title:"json", payload});
            if (JSON.stringify(inserted.payload) !== JSON.stringify(payload)) {
                throw new Error("insert changed the JSON type");
            }
            const updated = await tx.notes.update({id:inserted.id}, {payload:{$set:payload}});
            if (JSON.stringify(updated.payload) !== JSON.stringify(payload)) {
                throw new Error("update changed the JSON type");
            }
        }
        return {preserved:true};
    });
    if (result.error) throw result.error;
    return result.data;
}
jsonValues.config = {kind:"action"};
const _procedures = {jsonValues};
"#,
    );
    let (status, body) =
        dispatch_zs_with_descriptor(&url, &source, "jsonValues", app, descriptor.to_string());
    assert_eq!(status, 200, "worker JSON values: {body}");
    assert_eq!(body["json"], serde_json::json!({"preserved":true}));
}

#[test]
fn timestamps_round_trip_through_worker_transactions() {
    let url = require_pg();
    let app = crate::test_app_id!();
    let app = app.as_str();
    reset_schema(&url, app);
    let role = zeroship_core::database_role::per_app_role_name(app).unwrap();
    exec_owner_sql(
        &url,
        &format!(
            "ALTER TABLE \"{app}\".notes ADD COLUMN instant TIMESTAMPTZ; \
             GRANT SELECT, INSERT, UPDATE ON \"{app}\".notes TO \"{role}\""
        ),
    );
    let mut descriptor: serde_json::Value =
        serde_json::from_str(&notes_runtime_descriptor()).unwrap();
    descriptor["collections"]["notes"]["fields"]["instant"] =
        serde_json::json!({"type":"timestamp", "nullable":true});
    let source = build_src(
        r#"
async function timestamps() {
    const instants = [
        [253402300790001, "9999-12-31T23:59:50.001Z"],
        [-62135596800000, "0001-01-01"],
        [-1, "1969-12-31T23:59:59.999999Z"],
        [0, "1970-01-01T02:00:00+02:00"],
        [253402300799999, "9999-12-31T23:59:59.999Z"],
    ];
    const result = await env.db.transaction(async tx => {
        for (const [millis, text] of instants) {
            for (const instant of [millis, text, new Date(millis)]) {
                const inserted = await tx.notes.insert({title:"timestamp", instant});
                if (inserted.instant !== millis) throw new Error("insert changed the instant");
                const updated = await tx.notes.update({id:inserted.id}, {instant:{$set:instant}});
                if (updated.instant !== millis) throw new Error("update changed the instant");
                const found = await tx.notes.find({id:inserted.id, instant:millis});
                if (found.length !== 1 || found[0].instant !== millis) throw new Error("timestamp filter failed");
            }
        }
        return {preserved:true};
    });
    if (result.error) throw result.error;
    for (const instant of ["private_not_a_timestamp", "2026-02-30T00:00:00Z", 0.5, 253402300800000]) {
        let refused = false;
        try {
            await env.db.collection("notes").insert({title:"invalid", instant});
        } catch (error) {
            if (error.code !== "invalid_timestamp") throw error;
            if (error.message.includes("private_not_a_timestamp")) throw new Error("validation leaked input");
            refused = true;
        }
        if (!refused) throw new Error("native timestamp validation was bypassed");
    }
    return result.data;
}
timestamps.config = {kind:"action"};
const _procedures = {timestamps};
"#,
    );
    let (status, body) =
        dispatch_zs_with_descriptor(&url, &source, "timestamps", app, descriptor.to_string());
    assert_eq!(status, 200, "worker timestamps: {body}");
    assert_eq!(body["json"], serde_json::json!({"preserved":true}));
    assert_eq!(count_notes(&url, app), 15);
}

#[test]
fn nested_timestamps_follow_worker_descriptors() {
    let url = require_pg();
    let app = crate::test_app_id!();
    let app = app.as_str();
    reset_schema(&url, app);
    let role = zeroship_core::database_role::per_app_role_name(app).unwrap();
    exec_owner_sql(&url, &format!(
        "ALTER TABLE \"{app}\".notes ADD COLUMN instants JSONB, ADD COLUMN profile JSONB, ADD COLUMN payload JSONB; \
         GRANT SELECT, INSERT, UPDATE ON \"{app}\".notes TO \"{role}\""
    ));
    let mut descriptor: serde_json::Value =
        serde_json::from_str(&notes_runtime_descriptor()).unwrap();
    descriptor["collections"]["notes"]["fields"]["instants"] =
        serde_json::json!({"type":"array","items":"date"});
    descriptor["collections"]["notes"]["fields"]["profile"] =
        serde_json::json!({"type":"object","shape":{"instant":{"type":"timestamp"}}});
    descriptor["collections"]["notes"]["fields"]["payload"] = serde_json::json!({"type":"json"});
    let source = build_src(
        r#"
async function runNestedTimestamps() {
    const iso = "1969-12-31T23:59:59.999Z";
    const result = await env.db.transaction(async tx => {
        for (const instant of [iso, new Date(-1), -1]) {
            const row = await tx.notes.insert({title:"nested", instants:[instant], profile:{instant}, payload:{instant:new Date(-1)}});
            if (row.instants[0] !== -1 || row.profile.instant !== -1) throw new Error("nested timestamp changed type");
            if (row.payload.instant !== iso) throw new Error("untyped JSON Date changed");
            const found = await tx.notes.find({id:row.id, instants:{$eq:[new Date(-1)]}});
            if (found.length !== 1) throw new Error("array equality did not normalize Date");
            const updated = await tx.notes.update({id:row.id}, {instants:{$push:new Date(0)}});
            if (JSON.stringify(updated.instants) !== "[-1,0]") throw new Error("timestamp push changed type");
            await tx.notes.update({id:row.id}, {instants:{$pull:"1970-01-01T01:00:00+01:00"}});
        }
        return {preserved:true};
    });
    if (result.error) throw result.error;
    for (const document of [
        {instants:["private_not_a_timestamp"]},
        {profile:{instant:"2026-02-30"}},
    ]) {
        let refused = false;
        try { await env.db.collection("notes").insert({title:"invalid", ...document}); }
        catch (error) {
            if (error.code !== "invalid_timestamp") throw error;
            if (error.message.includes("private_not_a_timestamp")) throw new Error("validation leaked input");
            refused = true;
        }
        if (!refused) throw new Error("native nested timestamp validation was bypassed");
    }
    return result.data;
}
async function nestedTimestamps() {
    try { return await runNestedTimestamps(); }
    catch (error) { return {failure:error.message, code:error.code, stack:error.stack}; }
}
nestedTimestamps.config = {kind:"action"};
const _procedures = {nestedTimestamps};
"#,
    );
    let (status, body) = dispatch_zs_with_descriptor(
        &url,
        &source,
        "nestedTimestamps",
        app,
        descriptor.to_string(),
    );
    assert_eq!(status, 200, "worker nested timestamps: {body}");
    assert_eq!(body["json"], serde_json::json!({"preserved":true}));
}

#[test]
fn array_updates_preserve_worker_json_elements() {
    let url = require_pg();
    let app = crate::test_app_id!();
    let app = app.as_str();
    reset_schema(&url, app);
    let role = zeroship_core::database_role::per_app_role_name(app).unwrap();
    exec_owner_sql(&url, &format!(
        "ALTER TABLE \"{app}\".notes ADD COLUMN items JSONB; \
         GRANT SELECT, INSERT, UPDATE ON \"{app}\".notes TO \"{role}\""
    ));
    let mut descriptor: serde_json::Value =
        serde_json::from_str(&notes_runtime_descriptor()).unwrap();
    descriptor["collections"]["notes"]["fields"]["items"] =
        serde_json::json!({"type":"array","items":"json"});
    let source = build_src(
        r#"
async function arrays() {
    const result = await env.db.transaction(async tx => {
        const row = await tx.notes.insert({title:"arrays", items:[1,"1",true,{a:1,b:2}]});
        const updates = [
            [{$push:null}, [1,"1",true,{a:1,b:2},null]],
            [{$push:[2,3]}, [1,"1",true,{a:1,b:2},null,[2,3]]],
            [{$addToSet:{b:2,a:1}}, [1,"1",true,{a:1,b:2},null,[2,3]]],
            [{$addToSet:{a:1}}, [1,"1",true,{a:1,b:2},null,[2,3],{a:1}]],
            [{$pull:1}, ["1",true,{a:1,b:2},null,[2,3],{a:1}]],
            [{$pull:null}, ["1",true,{a:1,b:2},[2,3],{a:1}]],
            [{$pull:[2,3]}, ["1",true,{a:1,b:2},{a:1}]],
        ];
        for (const [items, expected] of updates) {
            const updated = await tx.notes.update({id:row.id}, {items});
            if (JSON.stringify(updated.items) !== JSON.stringify(expected)) {
                throw new Error(`array update changed JSON elements: ${JSON.stringify(updated.items)}`);
            }
        }
        return {preserved:true};
    });
    if (result.error) throw result.error;
    return result.data;
}
arrays.config = {kind:"action"};
const _procedures = {arrays};
"#,
    );
    let (status, body) =
        dispatch_zs_with_descriptor(&url, &source, "arrays", app, descriptor.to_string());
    assert_eq!(status, 200, "worker array updates: {body}");
    assert_eq!(body["json"], serde_json::json!({"preserved":true}));
}

#[test]
fn update_validation_is_shared_by_native_and_sdk_worker_calls() {
    let url = require_pg();
    let app = crate::test_app_id!();
    let app = app.as_str();
    reset_schema(&url, app);
    let role = zeroship_core::database_role::per_app_role_name(app).unwrap();
    exec_owner_sql(
        &url,
        &format!(
            "ALTER TABLE \"{app}\".notes ADD COLUMN balance DOUBLE PRECISION, ADD COLUMN payload JSONB; \
         GRANT SELECT, INSERT, UPDATE ON \"{app}\".notes TO \"{role}\""
        ),
    );
    let mut descriptor: serde_json::Value =
        serde_json::from_str(&notes_runtime_descriptor()).unwrap();
    descriptor["collections"]["notes"]["fields"]["balance"] = serde_json::json!({"type":"number"});
    descriptor["collections"]["notes"]["fields"]["payload"] = serde_json::json!({"type":"json"});
    let source = build_src(
        r#"
async function updates() {
    const native = env.db.collection("notes");
    const row = await native.insert({title:"validation", balance:10});
    for (const [patch, code] of [
        [{balance:{$inc:"2"}}, "invalid_arithmetic_operand"],
        [{balance:{$mul:null}}, "invalid_arithmetic_operand"],
        [{balance:{$inc:1,$mul:2}}, "invalid_update"],
        [{$set:{balance:1},balance:2}, "invalid_update"],
    ]) {
        let refused = false;
        try { await native.update({id:row.id}, patch); }
        catch (error) { if (error.code !== code) throw error; refused = true; }
        if (!refused) throw new Error("native invalid update succeeded");
    }
    const result = await env.db.transaction(async tx => {
        let refused = false;
        try { await tx.notes.update({id:row.id}, {$inc:{balance:1},$mul:{balance:2}}); }
        catch { refused = true; }
        if (!refused) throw new Error("SDK discarded a conflicting assignment");
        const updated = await tx.notes.update({id:row.id}, {
            $set:{payload:{$inc:2,description:"literal JSON"}}, $inc:{balance:1},
        });
        if (updated.balance !== 11 || updated.version !== 2 || updated.payload.$inc !== 2) {
            throw new Error(`update changed semantics: ${JSON.stringify(updated)}`);
        }
        return {validated:true};
    });
    if (result.error) throw result.error;
    return result.data;
}

updates.config = {kind:"action"};
const _procedures = {updates};
"#,
    );
    let (status, body) =
        dispatch_zs_with_descriptor(&url, &source, "updates", app, descriptor.to_string());
    assert_eq!(status, 200, "worker update validation: {body}");
    assert_eq!(body["json"], serde_json::json!({"validated":true}));
}

#[test]
fn native_worker_calls_validate_array_item_types() {
    let url = require_pg();
    let app = crate::test_app_id!();
    let app = app.as_str();
    reset_schema(&url, app);
    let role = zeroship_core::database_role::per_app_role_name(app).unwrap();
    exec_owner_sql(
        &url,
        &format!(
            "ALTER TABLE \"{app}\".notes ADD COLUMN names JSONB; \
         GRANT SELECT, INSERT, UPDATE ON \"{app}\".notes TO \"{role}\""
        ),
    );
    let mut descriptor: serde_json::Value =
        serde_json::from_str(&notes_runtime_descriptor()).unwrap();
    descriptor["collections"]["notes"]["fields"]["names"] =
        serde_json::json!({"type":"array","items":"string"});
    let source = build_src(
        r#"
async function arrayTypes() {
    const native = env.db.collection("notes");
    const row = await native.insert({title:"arrays", names:["original"]});
    const mutations = [
        () => native.insert({title:"invalid", names:[true]}),
        () => native.insertMany([{title:"valid", names:[]}, {title:"invalid", names:[true]}]),
        () => native.update({id:row.id}, {names:{$set:[true]}}),
        () => native.update({id:row.id}, {names:{$push:1}}),
        () => native.updateMany({}, {names:{$addToSet:false}}),
        () => native.updateMany({title:"missing"}, {names:{$pull:1}}),
    ];
    for (const mutate of mutations) {
        let refused = false;
        try { await mutate(); }
        catch (error) {
            if (error.code !== "invalid_array_element") throw error;
            refused = true;
        }
        if (!refused) throw new Error("invalid native array mutation succeeded");
    }
    const updated = await native.update({id:row.id}, {names:{$push:"next"}});
    if (updated.version !== 2 || JSON.stringify(updated.names) !== '["original","next"]') {
        throw new Error("invalid mutations changed the row");
    }
    const rows = await native.find({});
    if (rows.length !== 1) throw new Error("invalid insert left rows behind");
    return {validated:true};
}
arrayTypes.config = {kind:"action"};
const _procedures = {arrayTypes};
"#,
    );
    let (status, body) =
        dispatch_zs_with_descriptor(&url, &source, "arrayTypes", app, descriptor.to_string());
    assert_eq!(status, 200, "native worker array validation: {body}");
    assert_eq!(body["json"], serde_json::json!({"validated":true}));
}

#[test]
fn calendar_dates_round_trip_through_worker_transactions() {
    let url = require_pg();
    let app = crate::test_app_id!();
    let app = app.as_str();
    reset_schema(&url, app);
    let role = zeroship_core::database_role::per_app_role_name(app).unwrap();
    exec_owner_sql(
        &url,
        &format!(
            "ALTER TABLE \"{app}\".notes ADD COLUMN birthday DATE; \
         GRANT SELECT, INSERT, UPDATE ON \"{app}\".notes TO \"{role}\""
        ),
    );
    let mut descriptor: serde_json::Value =
        serde_json::from_str(&notes_runtime_descriptor()).unwrap();
    descriptor["collections"]["notes"]["fields"]["birthday"] =
        serde_json::json!({"type":"calendarDate", "nullable":true});
    let source = build_src(
        r#"
async function calendarDates() {
    const dates = ["0001-01-01", "0004-02-29", "0099-12-31", "0100-03-01", "1969-12-31", "2000-02-29", "9999-12-31"];
    const result = await env.db.transaction(async tx => {
        for (const birthday of dates) {
            const inserted = await tx.notes.insert({title:"calendar date", birthday});
            if (inserted.birthday !== birthday) throw new Error("insert changed the calendar date");
            const updated = await tx.notes.update({id:inserted.id}, {birthday:{$set:birthday}});
            if (updated.birthday !== birthday) throw new Error("update changed the calendar date");
        }
        return {preserved:true};
    });
    if (result.error) throw result.error;
    let refused = false;
    try {
        // Exercise the native boundary directly, without the SDK validator.
        await env.db.collection("notes").insert({title:"invalid", birthday:"2026-02-30"});
    } catch (error) {
        if (error.code !== "invalid_calendar_date") throw error;
        refused = true;
    }
    if (!refused) throw new Error("native calendar date validation was bypassed");
    return {...result.data, refused};
}
calendarDates.config = {kind:"action"};
const _procedures = {calendarDates};
"#,
    );
    let (status, body) =
        dispatch_zs_with_descriptor(&url, &source, "calendarDates", app, descriptor.to_string());
    assert_eq!(status, 200, "worker calendar dates: {body}");
    assert_eq!(
        body["json"],
        serde_json::json!({"preserved":true, "refused":true})
    );
    assert_eq!(count_notes(&url, app), 7);
}

#[test]
fn worker_upserts_preserve_platform_identity_and_reject_invalid_conflict_keys() {
    let url = require_pg();
    let app = crate::test_app_id!();
    let app = app.as_str();
    reset_schema(&url, app);
    exec_owner_sql(
        &url,
        &format!("CREATE UNIQUE INDEX notes_identity_title ON \"{app}\".notes (title)"),
    );
    let source = build_src(
        r#"
async function identities() {
    const raw = env.db.collection("notes");
    const refused = async (operation, code) => {
        try { await operation(); }
        catch (error) {
            if (error.code !== code) throw error;
            return;
        }
        throw new Error("invalid identity write was accepted");
    };
    await refused(() => raw.insert({id:"usr_caller_chosen",title:"blocked"}), "platform_assigned_field");
    await refused(() => raw.insertMany([{title:"valid"},{id:"usr_caller_chosen",title:"blocked"}]), "platform_assigned_field");
    await refused(() => raw.upsert({id:"usr_caller_chosen",title:"blocked"},{conflictFields:["title"]}), "platform_assigned_field");
    await refused(() => raw.upsert({title:"blocked"},{conflictFields:["id"]}), "platform_assigned_conflict_field");
    const result = await env.db.transaction(async tx => {
        const first = await tx.notes.upsert({title:"key"},{conflictFields:["title"]});
        const second = await tx.notes.upsert({title:"key"},{conflictFields:["title"]});
        if (first.id !== second.id || !first.id.startsWith("note_")) throw new Error("upsert changed identity");
        const updated = await tx.notes.update(first.id,{title:"renamed"});
        if (updated.id !== first.id) throw new Error("update changed identity");
        return {preserved:true};
    });
    if (result.error) throw result.error;
    const invalid = await env.db.transaction(tx => tx.notes.upsert({title:"blocked"},{conflictFields:["id"]}));
    if (!invalid.error) throw new Error("SDK accepted an assigned conflict key");
    return result.data;
}
identities.config = {kind:"action"};
const _procedures = {identities};
"#,
    );
    let (status, body) = dispatch_zs(&url, &source, "identities", app);
    assert_eq!(status, 200, "worker identities: {body}");
    assert_eq!(body["json"], serde_json::json!({"preserved":true}));
    assert_eq!(count_notes(&url, app), 1);
}

#[test]
fn transaction_commits_on_resolve() {
    let url = require_pg();
    let app = crate::test_app_id!();
    let app = app.as_str();
    reset_schema(&url, app);

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
const _procedures = { commitOne };
"#,
    );

    let (status, body) = dispatch_zs(&url, &src, "commitOne", app);
    assert_eq!(status, 200, "commitOne failed: {body}");
    let inner = body.get("json").cloned().unwrap_or(serde_json::Value::Null);
    assert_eq!(
        inner
            .get("txResult")
            .and_then(|v| v.get("data"))
            .and_then(|v| v.as_str()),
        Some("ok"),
        "transaction(fn) must resolve with the callback's return value; body={body}"
    );
    assert_eq!(count_notes(&url, app), 1, "committed row must be visible");
}

/// Rollback on async reject: the callback inserts then throws; nothing
/// persists, and `transaction(fn)` rejects with the thrown error.
#[test]
fn transaction_rolls_back_on_async_reject() {
    let url = require_pg();
    let app = crate::test_app_id!();
    let app = app.as_str();
    reset_schema(&url, app);

    let src = build_src(
        r#"
async function insertThenThrow(_input, _ctx) {
    // `transaction()` returns a Result and never throws (see the module
    // header). The creator's thrown error arrives as `result.error`, so read
    // it off the envelope. `message` is a non-enumerable own property of
    // Error, so JSON.stringify would drop it - copy it out explicitly.
    const r = await env.db.transaction(async (tx) => {
        await tx.notes.insert({ title: "rolledBack" });
        throw Object.assign(new Error("abort it"), { code: "user_abort" });
    });
    return {
        threw: false,
        data: r.data ?? null,
        message: r.error?.message ?? null,
        code: r.error?.code ?? null,
    };
}
insertThenThrow.config = { kind: "action" };
const _procedures = { insertThenThrow };
"#,
    );

    let (status, body) = dispatch_zs(&url, &src, "insertThenThrow", app);
    assert_eq!(status, 200, "harness should succeed: {body}");
    let inner = body.get("json").cloned().unwrap_or(serde_json::Value::Null);
    assert_eq!(
        inner.get("message").and_then(|v| v.as_str()),
        Some("abort it"),
        "transaction(fn) must surface the thrown error verbatim on result.error; body={body}"
    );
    assert_eq!(
        inner.get("code").and_then(|v| v.as_str()),
        Some("user_abort"),
        "the thrown error's custom .code must survive the rollback path; body={body}"
    );
    assert!(
        inner.get("data").is_some_and(serde_json::Value::is_null),
        "a rolled-back transaction must carry no data; body={body}"
    );
    // THE REAL PRODUCT CLAIM, and it had never executed: the old assertion
    // panicked on the envelope shape before reaching this line.
    assert_eq!(count_notes(&url, app), 0, "rolled-back tx must not persist");
}

/// Rollback on a synchronous throw inside the callback (the callback is
/// not `async` and throws before returning a Promise — captured by the
/// orchestrator's TryCatch).
#[test]
fn transaction_sync_throw_in_callback_rolls_back() {
    let url = require_pg();
    let app = crate::test_app_id!();
    let app = app.as_str();
    reset_schema(&url, app);

    let src = build_src(
        r#"
async function syncThrow(_input, _ctx) {
    const r = await env.db.transaction((tx) => {
        // Not async — throws synchronously before any Promise exists.
        throw new Error("sync boom");
    });
    return { message: r.error?.message ?? null, data: r.data ?? null };
}
syncThrow.config = { kind: "action" };
const _procedures = { syncThrow };
"#,
    );

    let (status, body) = dispatch_zs(&url, &src, "syncThrow", app);
    assert_eq!(status, 200, "harness should succeed: {body}");
    let inner = body.get("json").cloned().unwrap_or(serde_json::Value::Null);
    assert_eq!(
        inner.get("message").and_then(|v| v.as_str()),
        Some("sync boom"),
        "a synchronous throw inside the callback must land on result.error; body={body}"
    );
    assert_eq!(count_notes(&url, app), 0);
}

/// Nested transaction emits a SAVEPOINT: the inner `transaction()` fails,
/// rolling back ONLY its own write (ROLLBACK TO SAVEPOINT); the outer
/// catches the inner failure, inserts its own row, and commits. Only the
/// outer row persists — proving the savepoint isolated the inner failure
/// without poisoning the whole tx.
#[test]
fn nested_inner_reject_rolls_back_to_savepoint_outer_continues() {
    let url = require_pg();
    let app = crate::test_app_id!();
    let app = app.as_str();
    reset_schema(&url, app);

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
const _procedures = { nestedPartialFailure };
"#,
    );

    let (status, body) = dispatch_zs(&url, &src, "nestedPartialFailure", app);
    assert_eq!(status, 200, "nested handler should succeed: {body}");
    let inner = body.get("json").cloned().unwrap_or(serde_json::Value::Null);
    assert_eq!(
        inner
            .get("txResult")
            .and_then(|v| v.get("data"))
            .and_then(|v| v.as_str()),
        Some("outer-committed"),
        "outer tx must commit after the inner savepoint rolled back; body={body}"
    );
    // Exactly the outer row persists — the inner one was rolled back to
    // the savepoint, the outer one committed.
    assert_eq!(
        count_notes(&url, app),
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
    let app = crate::test_app_id!();
    let app = app.as_str();
    reset_schema(&url, app);

    // Same thread as `block_on`'s runtime (`RT.with`), so this shares the
    // thread-local broker the dispatch path publishes into.
    let sub = zeroship_data_orm::broker::subscribe(app, "notes");

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
const _procedures = { savepointEmitLeak };
"#,
    );

    let (status, body) = dispatch_zs(&url, &src, "savepointEmitLeak", app);
    assert_eq!(status, 200, "handler should succeed: {body}");

    // Ground truth: exactly one row survived.
    assert_eq!(
        count_notes(&url, app),
        1,
        "precondition: only the outer row persists; body={body}"
    );

    let mut published: Vec<String> = Vec::new();
    while let Some(msg) = sub.pop() {
        if let zeroship_data_orm::broker::SubscriptionMessage::Change(ev) = msg {
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
    let app = crate::test_app_id!();
    let app = app.as_str();
    reset_schema(&url, app);

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
const _procedures = { nestedBothCommit };
"#,
    );

    let (status, body) = dispatch_zs(&url, &src, "nestedBothCommit", app);
    assert_eq!(status, 200, "nested handler should succeed: {body}");
    let inner = body.get("json").cloned().unwrap_or(serde_json::Value::Null);
    assert_eq!(
        inner
            .get("txResult")
            .and_then(|v| v.get("data"))
            .and_then(|v| v.as_str()),
        Some("both-ok")
    );
    assert_eq!(
        count_notes(&url, app),
        2,
        "both inner (RELEASE SAVEPOINT) and outer (COMMIT) writes must persist; body={body}"
    );
}

/// Savepoint depth cap: nesting `transaction()` past MAX_SAVEPOINT_DEPTH
/// (`crates/zeroship-data-orm/src/transaction/mod.rs`, = 8) refuses the 9th
/// savepoint with `savepoint_depth_exceeded`.
///
/// NAMED `..._is_refused`, not `..._throws`: the refusal arrives as
/// `result.error` on the level that could not open its savepoint, because
/// `env.db.transaction` here is the bootstrap wrapper (module header). It
/// asserted a throw until 2026-09-04 and had never once observed the cap - it
/// panicked on `tripped: false` while the very same body reported
/// `reachedLevel: 10`, which is the cap doing exactly its job.
#[test]
fn savepoint_depth_cap_8_exceeded_is_refused() {
    let url = require_pg();
    let app = crate::test_app_id!();
    let app = app.as_str();
    reset_schema(&url, app);

    // Recurse `transaction()` to depth `n`. The outermost is the BEGIN
    // (depth 0 savepoints); each nested call is one savepoint. The 9th
    // *savepoint* (i.e. the 10th transaction() level) must throw. We open
    // 1 BEGIN + N savepoints; choosing N = 9 trips the cap.
    let src = build_src(
        r#"
async function deepNest(_input, _ctx) {
    let level = 0;
    let refusal = null;
    async function go() {
        level += 1;
        if (level > 12) return; // safety stop (should never reach)
        const r = await env.db.transaction(async () => {
            await go();
        });
        // The recursion unwinds innermost-first, so the FIRST error observed
        // is the one from the level that could not open its savepoint. Every
        // enclosing level's callback resolves normally afterwards and its own
        // RELEASE SAVEPOINT / COMMIT succeeds, so exactly one level reports.
        if (r.error && refusal === null) {
            refusal = { code: r.error.code ?? null, atLevel: level };
        }
    }
    await go();
    return { tripped: refusal !== null, refusal, reachedLevel: level };
}
deepNest.config = { kind: "action" };
const _procedures = { deepNest };
"#,
    );

    let (status, body) = dispatch_zs(&url, &src, "deepNest", app);
    assert_eq!(
        status, 200,
        "the wrapper returns the depth-cap refusal as data: {body}"
    );
    let inner = body.get("json").cloned().unwrap_or(serde_json::Value::Null);
    assert_eq!(
        inner.get("tripped").and_then(|v| v.as_bool()),
        Some(true),
        "deep nesting must trip the savepoint depth cap; body={body}"
    );
    assert_eq!(
        inner.pointer("/refusal/code").and_then(|v| v.as_str()),
        Some("savepoint_depth_exceeded"),
        "the depth-cap refusal must carry code=savepoint_depth_exceeded; body={body}"
    );
    // THE ARITHMETIC, pinned rather than implied. Level 1 is the BEGIN and
    // opens no savepoint; levels 2..=9 open savepoints 1..=8, which is
    // MAX_SAVEPOINT_DEPTH; level 10 would be savepoint 9 and is refused. So
    // the refusal must land at level 10 and the recursion must stop there,
    // well short of the JS safety stop at 12.
    assert_eq!(
        inner.pointer("/refusal/atLevel").and_then(|v| v.as_u64()),
        Some(10),
        "the 9th savepoint is the 10th transaction() level; body={body}"
    );
    assert_eq!(
        inner.get("reachedLevel").and_then(|v| v.as_u64()),
        Some(10),
        "the cap must stop the recursion, not the JS safety stop; body={body}"
    );
}

/// The `tx` view handed to the callback is collections-only: it has no
/// `commit` / `rollback` / `collection` method.
#[test]
fn tx_view_has_no_lifecycle_methods() {
    let url = require_pg();
    let app = crate::test_app_id!();
    let app = app.as_str();
    reset_schema(&url, app);

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
const _procedures = { probeTxView };
"#,
    );

    let (status, body) = dispatch_zs(&url, &src, "probeTxView", app);
    assert_eq!(status, 200, "probe should succeed: {body}");
    // `transaction()` resolves with the `{ data, error }` envelope documented at
    // `sdks/db/src/db-types.ts:39`, so the probe's own object sits under `data`.
    let inner = body
        .get("json")
        .and_then(|v| v.get("data"))
        .cloned()
        .unwrap_or(serde_json::Value::Null);
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
    let app = crate::test_app_id!();
    let app = app.as_str();
    reset_schema(&url, app);

    let src = build_src(
        r#"
function probeBegin(_input, _ctx) {
    return { beginType: typeof env.db.beginTransaction };
}
probeBegin.config = { kind: "action" };
const _procedures = { probeBegin };
"#,
    );

    let (status, body) = dispatch_zs(&url, &src, "probeBegin", app);
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
    let app = crate::test_app_id!();
    let app = app.as_str();
    reset_schema(&url, app);
    let key_id = "update_many_atomic_pg";
    create_encrypted_users_table(&url, app, key_id);
    let _keys = zeroship_data_v8::supply_root_keys_for_tests(&[(key_id, &"b".repeat(64))]);

    let src = build_encrypted_users_src(
        r#"
async function seed(_input, _ctx) {
    const coll = env.db.collection("users");
    try {
        // No `id` on either row: it is platform-assigned, so supplying one is
        // refused at the document boundary. Nothing below reads these ids -
        // the assertions filter on `name` - so they were fixture convenience.
        await coll.insert({
            email: "alice@example.com",
            name: "Red Team",
            ssn: "123-45-6789"
        });
        await coll.insert({
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

const _procedures = { seed, failBulk };
"#,
    );

    let (status, body) =
        dispatch_zs_with_descriptor(&url, &src, "seed", app, users_runtime_descriptor(key_id));
    assert_eq!(status, 200, "seed failed: {body}");
    assert!(body["json"]["failure"].is_null(), "seed failed: {body}");

    zeroship_data_orm::crud::reset_write_path_counters_for_tests();
    let (status, body) = dispatch_zs_with_descriptor(
        &url,
        &src,
        "failBulk",
        app,
        users_runtime_descriptor(key_id),
    );
    assert_eq!(
        status, 200,
        "caught updateMany failure must remain inspectable: {body}"
    );
    let result = &body["json"];
    assert_eq!(result["failure"]["code"], "unique_violation", "body={body}");
    let after = result["after"]
        .as_array()
        .expect("caller-visible post-failure rows must be an array");
    assert_eq!(
        after.len(),
        2,
        "the exercised target set must be non-empty: {body}"
    );
    let counters = zeroship_data_orm::crud::write_path_counters_for_tests();
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
        zeroship_data_sql::compile::MAX_QUERY_LIMIT + 1
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
        user_email_versions(&url, app),
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
/// The settle path reads that tag. `transaction::driver::terminal` sends
/// terminal SQL through `batch_execute_reporting_tag` precisely so the tag
/// survives, and classifies `(Commit, Some("ROLLBACK"))` as
/// `TerminalResult::RolledBack`.
///
/// **The two paragraphs above replaced a description of the pre-fix world.**
/// The old text said the settle path sends its COMMIT through `execute_fixture_on`,
/// which throws the tag away. It does not, and
/// `transaction/mod.rs`'s own header says so in as many words: terminal
/// statements deliberately bypass `execute_fixture_on`. The classifier landed and this
/// comment did not move.
///
/// SCOPE: this drives an EXPLICIT creator transaction on purpose. The autocommit
/// path goes through the driver's own `tx.commit()` wrapper, which checks the
/// tag, so the same test written against autocommit proves nothing about this
/// path.
///
/// ## Poisoning the transaction, and why the obvious way does not work
///
/// The callback must swallow a genuine SERVER-SIDE error so it RESOLVES; the
/// orchestrator then commits a poisoned transaction, which is the state under
/// test.
///
/// This test inserted the same `id` twice until 2026-09-02, expecting a
/// duplicate-key violation. **It never got one.** `id` is platform-assigned: a
/// creator-supplied value is discarded and a typed id minted in its place, so
/// both inserts succeeded with different keys, the transaction committed
/// cleanly, and the assertion failed against a HEALTHY commit while its message
/// claimed "the row is gone (count=2)". Measured on the live server, the two
/// surviving rows were `note_034HvhQfm7VR3Q3olk7zxc` and
/// `note_034HvhQfTvtV3JZE1b9KdH`, not `l8-keep`. Same defect as the one closed
/// on `#125`, in a second test.
///
/// The poison therefore has to be a constraint on a column the platform does
/// NOT rewrite. `title` is creator data, so a unique index on it produces a real
/// `23505` the runtime cannot absorb. The index is created here rather than in
/// `reset_schema` because every other test in this file inserts duplicate
/// titles freely.
#[test]
fn commit_that_postgres_rolled_back_must_not_report_success_l8() {
    let url = require_pg();
    let app = crate::test_app_id!();
    let app = app.as_str();
    reset_schema(&url, app);

    // The poison. `title` is creator data and survives the write path intact, so
    // a duplicate here is a real 23505 - unlike a duplicate `id`, which the
    // platform silently makes unique.
    exec_owner_sql(
        &url,
        &format!("CREATE UNIQUE INDEX \"notes_title_l8_uniq\" ON \"{app}\".\"notes\" (\"title\")"),
    );

    let src = build_src(
        r#"
async function poisonThenCommit(_input, _ctx) {
    const r = await env.db.transaction(async (tx) => {
        await tx.notes.insert({ title: "l8-keep" });
        try {
            // Duplicate TITLE against the unique index the fixture added: a real
            // server-side 23505, which puts the transaction into the failed
            // state where COMMIT answers ROLLBACK. Duplicating `id` does NOT
            // work - see this test's doc comment.
            await tx.notes.insert({ title: "l8-keep" });
        } catch (_e) {
            // Swallowed on purpose so the callback resolves.
        }
        return "ok";
    });
    return { txResult: r };
}
poisonThenCommit.config = { kind: "action" };
const _procedures = { poisonThenCommit };
"#,
    );

    let (status, body) = dispatch_zs(&url, &src, "poisonThenCommit", app);
    let rows = count_notes(&url, app);

    // SUCCESS MEANS DURABLE, in both directions. What must never happen is a
    // reported success whose writes are not there.
    //
    // **Success is the envelope's `error`, not the HTTP status.** An RPC that
    // rejects still answers 200 and carries the failure in the body, so
    // `status == 200` reads a failed commit as a successful one - which is how
    // this arm reported "commit reported SUCCESS" while the body plainly said
    // `{"error":{"code":"commit_rolled_back"}}`.
    let tx_error = body
        .pointer("/json/txResult/error")
        .filter(|error| !error.is_null());

    if tx_error.is_none() {
        assert_eq!(
            rows, 1,
            "commit reported SUCCESS (status {status}, body={body}) but PostgreSQL \
             rolled the transaction back and the row is gone (count={rows}). \
             A rolled-back commit must not be reported as committed."
        );
    } else {
        assert_eq!(
            rows, 0,
            "a failed commit must leave nothing behind; body={body}"
        );
    }
}

// ===========================================================================
// SC-1 reducer: the live-PostgreSQL oracles its model rests on
// ===========================================================================

/// Live arms binding the SC-1 reducer's model to a real server.
///
/// The reducer (`zeroship_data_orm::transaction::reducer`) is **pure** - it
/// owns no session and issues no SQL - so a "live reducer test" would be a
/// contradiction. What a live arm can and must prove is narrower: that the
/// three **server behaviours the reducer models** are real on the server we
/// ship against. Each arm below, if PostgreSQL behaved otherwise, would
/// invalidate one specific modelling decision, and each names which.
///
/// This module reuses the target's existing `pg_url()` - which is
/// `zeroship_core::config::test_database_url()`, the typed accessor - so it
/// introduces no environment variable of its own and calls no `set_var`.
///
/// Run with:
///
/// ```text
/// cargo test -p zeroship-data-v8 --features test-helpers --test test_helpers \
///   -- --test-threads=1 native_transaction::sc1_live
/// ```
///
/// Everything these arms create is a `TEMP` table, which dies with the
/// connection: nothing is left in the shared database to drop.
mod sc1_live {
    use compio_postgres::{Client, NoTls, TransactionStatus};
    use zeroship_data_orm::transaction::reducer::{
        CleanupAck, CleanupGoal, SettleIntent, TerminalOutcome, TerminalResult,
    };

    use super::{block_on, pg_url};

    /// Connect, and report the server actually reached.
    ///
    /// **Prints `server_version_num`, not a container tag.** A cross-version
    /// claim published off the variable rather than the server is a recorded
    /// failure in this repository; the number below comes from the session
    /// that ran the assertions.
    async fn connect() -> Client {
        let url = pg_url();
        let (client, connection) = compio_postgres::connect(&url, NoTls)
            .await
            .unwrap_or_else(|e| panic!("sc1_live needs a live PostgreSQL at {url}: {e}"));
        compio::runtime::spawn(async move {
            let _ = connection.run().await;
        })
        .detach();
        let version: String = client
            .query_one("SELECT current_setting('server_version_num')", &[])
            .await
            .expect("read server_version_num")
            .get(0);
        println!("sc1_live oracle: server_version_num={version}");
        client
    }

    /// Project PostgreSQL's health oracle onto SC-1's acknowledgement.
    ///
    /// **`None` is `Indeterminate`, and that is the whole point of the
    /// mapping.** SC-1 states it directly - the driver's `None` "means
    /// *indeterminate* and is documented as such" - and it is the reason a
    /// cleanup goal that is not *proved* withdraws the session rather than
    /// guessing. This helper is shared by both halves of the arm below so the
    /// two cannot disagree about what a status means.
    fn ack_for(status: Option<TransactionStatus>) -> CleanupAck {
        match status {
            Some(TransactionStatus::Idle) => CleanupAck::RolledBack,
            Some(TransactionStatus::InTransaction | TransactionStatus::Failed) | None => {
                CleanupAck::Indeterminate
            }
        }
    }

    /// **The L8 oracle.** A `COMMIT` issued in a failed transaction block is
    /// answered with the command tag `ROLLBACK`.
    ///
    /// The reducer maps `(SettleIntent::Commit, TerminalResult::RolledBack)`
    /// to `TerminalOutcome::RolledBack`, which publishes nothing. That mapping
    /// only means anything if the server really answers this way.
    ///
    /// Where this would fail today: on a server that answers `COMMIT` to a
    /// poisoned commit, or errors instead of answering. Either would make the
    /// reducer's L8 arm model a behaviour that does not exist.
    #[test]
    fn a_commit_in_a_failed_transaction_is_answered_with_the_rollback_tag() {
        block_on(async {
            let client = connect().await;
            client.batch_execute("BEGIN").await.expect("BEGIN");
            client
                .batch_execute("CREATE TEMP TABLE sc1_live_probe (id int)")
                .await
                .expect("temp table");
            // Poison the block. A creator callback can swallow this and
            // resolve, which is what makes the COMMIT below reachable.
            let poisoned = client.batch_execute("SELECT 1 / 0").await;
            assert!(poisoned.is_err(), "the block must actually be poisoned");

            let tag = client
                .batch_execute_reporting_tag("COMMIT")
                .await
                .expect("COMMIT is accepted even from a failed block");
            assert_eq!(
                tag.as_deref(),
                Some("ROLLBACK"),
                "PostgreSQL answers a poisoned COMMIT with the tag ROLLBACK; the \
                 reducer's (Commit, RolledBack) -> RolledBack mapping models exactly this"
            );

            // Bind the observation to the reducer's own mapping, so the two
            // cannot drift apart silently.
            let modelled = match tag.as_deref() {
                Some("COMMIT") => TerminalResult::Committed,
                Some("ROLLBACK") => TerminalResult::RolledBack,
                _ => TerminalResult::Indeterminate,
            };
            assert_eq!(modelled, TerminalResult::RolledBack);
            assert!(
                !TerminalOutcome::RolledBack.publishes(),
                "a transaction PostgreSQL rolled back publishes nothing"
            );
            let _ = SettleIntent::Commit;
        });
    }

    /// **The savepoint-shadowing oracle.** `ROLLBACK TO SAVEPOINT` leaves the
    /// savepoint defined, and a reused name resolves to the **most recently
    /// established** one.
    ///
    /// This is the precondition monotonic savepoint naming removes. The
    /// orchestrator ships depth-derived names, so a leftover savepoint of a
    /// reused name shadows an enclosing frame and sends its rollback to the
    /// wrong scope.
    ///
    /// Where this would fail today: on a server that RELEASES a savepoint when
    /// rolling back to it. There the reuse would be harmless and
    /// `FrameStack`'s monotonic sequence would be solving a problem that does
    /// not exist. Step 2 is the one that decides it - a released savepoint
    /// makes the second `ROLLBACK TO` fail with `3B001`.
    #[test]
    fn a_rolled_back_savepoint_stays_defined_and_a_reused_name_shadows_it() {
        block_on(async {
            let client = connect().await;
            client.batch_execute("BEGIN").await.expect("BEGIN");
            client
                .batch_execute("CREATE TEMP TABLE sc1_live_probe (tag text)")
                .await
                .expect("temp table");

            // 1. Establish the OUTER savepoint under a depth-derived name.
            client
                .batch_execute("SAVEPOINT zs_sp_1")
                .await
                .expect("outer");
            client
                .batch_execute("INSERT INTO sc1_live_probe VALUES ('outer')")
                .await
                .expect("outer write");

            // 2. Roll back to it, then roll back to it AGAIN. The second call
            //    is decisive: had PostgreSQL released the savepoint, this
            //    errors with 3B001 invalid_savepoint_specification.
            client
                .batch_execute("ROLLBACK TO SAVEPOINT zs_sp_1")
                .await
                .expect("first rollback-to");
            client
                .batch_execute("ROLLBACK TO SAVEPOINT zs_sp_1")
                .await
                .expect(
                    "ROLLBACK TO deliberately LEAVES the savepoint defined; if this \
                     errored, depth-derived names would be safe and monotonic naming \
                     would be unnecessary",
                );

            // 3. Reuse the name at the same depth - what a depth-derived
            //    scheme does after the depth decrements. There are now TWO
            //    savepoints called zs_sp_1.
            client
                .batch_execute("SAVEPOINT zs_sp_1")
                .await
                .expect("inner");
            client
                .batch_execute("INSERT INTO sc1_live_probe VALUES ('inner')")
                .await
                .expect("inner write");

            // 4. RELEASE destroys the INNER one only. Had the name replaced
            //    rather than shadowed, no savepoint of that name would remain
            //    and the ROLLBACK TO below would fail.
            client
                .batch_execute("RELEASE SAVEPOINT zs_sp_1")
                .await
                .expect("release the inner one");
            client
                .batch_execute("ROLLBACK TO SAVEPOINT zs_sp_1")
                .await
                .expect(
                    "the enclosing savepoint of the same name is STILL THERE - the \
                     reused name shadowed it rather than replacing it, which is how an \
                     enclosing frame's rollback reaches the wrong scope",
                );

            client.batch_execute("ROLLBACK").await.expect("clean up");
        });
    }

    /// **The health oracle the cleanup goals read.** `transaction_status()`
    /// reports `Idle` / `InTransaction` / `Failed`, and the reducer's
    /// [`CleanupAck`] is a faithful projection of it.
    ///
    /// SC-1 fixes each cleanup goal's proof against this oracle, so an arm
    /// asserting only the reducer's own table would be checking the reducer
    /// against itself. This one maps the live status onto `CleanupAck` and
    /// asserts the goals accept exactly what SC-1 says.
    ///
    /// Where this would fail today: on a server that reports `Idle` inside a
    /// failed block - which would make `Poisoned` unobservable and
    /// `CleanupGoal::OpenTransaction` unprovable, so forced cleanup could
    /// never distinguish "rolled back" from "never opened".
    #[test]
    fn transaction_status_is_the_oracle_the_cleanup_goals_read() {
        block_on(async {
            let client = connect().await;

            assert_eq!(
                client.transaction_status(),
                Some(TransactionStatus::Idle),
                "a fresh session is not in a transaction"
            );

            client.batch_execute("BEGIN").await.expect("BEGIN");
            assert_eq!(
                client.transaction_status(),
                Some(TransactionStatus::InTransaction),
                "a confirmed BEGIN is what fixes CleanupGoal::OpenTransaction"
            );

            let poisoned = client.batch_execute("SELECT 1 / 0").await;
            assert!(poisoned.is_err());

            // **The read straight after a failed statement is a RACE, and the
            // arm asserts the guarantee rather than the outcome.** The driver
            // decrements its in-flight counter when the connection task
            // consumes the trailing `ReadyForQuery`, which is not the moment a
            // failed `await` returns - so this observation is `None`
            // (unresolved) or `Some(Failed)` depending on scheduling. Measured
            // here: both occur, `None` when the target runs the `sc1_live`
            // filter alone and `Some(Failed)` in a full-target run.
            //
            // Asserting either literal is an arm whose verdict is noise. What
            // is guaranteed - and what SC-1 actually needs - is the negative:
            // **it is never `Some(Idle)`**, so a poisoned block can never be
            // mistaken for a proved clean one. Returning a stale `Idle` here is
            // the bug the driver's `Option` signature exists to prevent.
            let immediately_after = client.transaction_status();
            assert_ne!(
                immediately_after,
                Some(TransactionStatus::Idle),
                "a poisoned block must never read as idle - that is what would \
                 make a caller skip its rollback and hand the next user of the \
                 connection an aborted transaction"
            );
            assert_ne!(
                ack_for(immediately_after),
                CleanupAck::RolledBack,
                "whichever side of the race this lands on, it must not prove \
                 CleanupGoal::OpenTransaction - an unproved cleanup withdraws"
            );

            // **A further FAILING statement is not a barrier either.** Inside a
            // poisoned block every data statement fails with 25P02, and a
            // failed statement leaves the byte exactly as unresolved as the
            // first one did. So there is no retry that makes the public oracle
            // answer while the block stays poisoned.
            let refused = client.batch_execute("SELECT 1").await;
            assert!(
                refused.is_err(),
                "PostgreSQL refuses every further data statement until the block \
                 ends - this is what makes TxState::Poisoned a real server-side \
                 state rather than a bookkeeping flag"
            );
            assert_ne!(
                client.transaction_status(),
                Some(TransactionStatus::Idle),
                "still poisoned, still never idle"
            );

            // Only a statement that SUCCEEDS resolves the byte, and inside a
            // failed block the terminal statement is the one that can.
            //
            // **This is a timing constraint on any driver wiring the reducer**,
            // and it is not in SC-1: forced cleanup must sample the oracle
            // AFTER its `ROLLBACK`, never before. A driver that samples on
            // entry to `Cancelling` reads `None`, gets `Indeterminate`, and
            // withdraws a connection that was about to be perfectly healthy.
            client.batch_execute("ROLLBACK").await.expect("ROLLBACK");
            let after = client.transaction_status();
            assert_eq!(after, Some(TransactionStatus::Idle));

            // Project the live status onto the reducer's acknowledgement, and
            // assert the goals accept exactly what SC-1's table says.
            let ack = ack_for(after);
            assert_eq!(ack, CleanupAck::RolledBack);
            assert!(
                CleanupGoal::OpenTransaction.is_proved_by(ack),
                "a confirmed rollback proves OpenTransaction"
            );
            assert!(CleanupGoal::AbortIfOpened.is_proved_by(ack));
            assert!(
                !CleanupGoal::OpenTransaction.is_proved_by(CleanupAck::Indeterminate),
                "an indeterminate oracle never proves a goal - it withdraws the session"
            );
            assert!(
                !CleanupGoal::NoTransaction.is_proved_by(CleanupAck::RolledBack),
                "'rolled back' contradicts 'BEGIN was never sent'"
            );
        });
    }
}

// ===========================================================================
// SC-1 driver: the actions, executed against a real server
// ===========================================================================

/// Arms binding the SC-1 **driver** to a real server.
///
/// The module above proves the server behaves the way the reducer models. This
/// one proves the driver acts on those behaviours correctly - it drives
/// `transaction::driver` through `transaction::probe` and asserts on the session
/// disposition, the pool, and the savepoint names that actually reached the
/// wire.
///
/// It reuses this target's `pg_url()` - `zeroship_core::config::test_database_url()`,
/// the typed accessor - so it introduces no environment variable of its own and
/// calls no `set_var`.
///
/// Every arm uses a unique `zs_sc1drv_*` app id and drops the role and schema it
/// created, so a shared server is left as it was found.
///
/// Run with:
///
/// ```text
/// cargo test -p zeroship-data-v8 --features test-helpers --test test_helpers \
///   -- --test-threads=1 native_transaction::sc1_driver
/// ```
mod sc1_driver {
    use compio_postgres::{Client, NoTls, Pool};
    use zeroship_data_orm::transaction::probe;
    use zeroship_data_orm::transaction::reducer::{
        CleanupCause, SessionOwnership, TerminalOutcome, TxState,
    };

    use super::{block_on, pg_url};

    /// The backend `probe::begin` takes as a parameter.
    ///
    /// It resolved its own through `tx_scope::ensure_backend` until 2026-09-03,
    /// which made an ENGINE file call the ADAPTER - the one direction the crate
    /// split forbids, and a hard cargo error once `transaction/` became
    /// `zeroship-data-orm`. The lookup lives here now, in the caller that
    /// owns the thread context, and it is the same call the V8 dispatcher makes
    /// on this test's behalf in production.
    /// The physical schema a probe opens its session against.
    ///
    /// Derived from the app id here because these fixtures still mint one
    /// string for both identities - the same thing production does today. The
    /// point of the parameter is that the CALL now states which it means.
    fn app_schema(app_id: &str) -> zeroship_data_sql::SchemaName {
        zeroship_data_sql::SchemaName::new(app_id).expect("fixture schema name")
    }

    async fn probe_backend() -> zeroship_data_orm::backend::BackendHandle {
        zeroship_data_v8::tx_scope::ensure_backend()
            .await
            .expect("the adapter funnel must open a backend before BEGIN")
    }

    /// Connect an out-of-band admin session, and report the server reached.
    ///
    /// **Prints `server_version_num`, not a container tag.** A cross-version
    /// claim published off the variable rather than the server is a recorded
    /// failure in this repository; the number below comes from the session that
    /// ran the assertions.
    async fn admin() -> Client {
        let url = pg_url();
        let (client, connection) = compio_postgres::connect(&url, NoTls)
            .await
            .unwrap_or_else(|e| panic!("sc1_driver needs a live PostgreSQL at {url}: {e}"));
        compio::runtime::spawn(async move {
            let _ = connection.run().await;
        })
        .detach();
        let version: String = client
            .query_one("SELECT current_setting('server_version_num')", &[])
            .await
            .expect("read server_version_num")
            .get(0);
        println!("sc1_driver oracle: server_version_num={version}");
        client
    }

    /// Provision the schema and per-app role the transaction session's
    /// `SET LOCAL ROLE` needs, and install the pool the driver checks out from.
    ///
    /// The pool is sized to **one** connection deliberately: with a single slot,
    /// "did the session come back" is answerable by taking the next checkout and
    /// comparing its backend PID, with no chance of being handed a different
    /// idle entry.
    async fn provision(app_id: &str) -> Client {
        let client = admin().await;
        let role = zeroship_core::database_role::per_app_role_name(app_id)
            .expect("transaction fixture app id must produce a valid PostgreSQL role name");
        client
            .batch_execute(&format!(
                "DROP SCHEMA IF EXISTS \"{app_id}\" CASCADE; \
                 CREATE SCHEMA \"{app_id}\"; \
                 DO $$ BEGIN \
                   IF NOT EXISTS (SELECT 1 FROM pg_roles WHERE rolname = '{role}') THEN \
                     CREATE ROLE \"{role}\" NOLOGIN NOREPLICATION; \
                   END IF; \
                 END $$; \
                 GRANT USAGE, CREATE ON SCHEMA \"{app_id}\" TO \"{role}\""
            ))
            .await
            .unwrap_or_else(|e| panic!("provision {app_id}: {e}"));

        let pool = Pool::connect(&pg_url(), 1)
            .await
            .expect("a one-connection pool for the driver to check out from");
        zeroship_data_v8::set_postgres_pool_for_tests(std::rc::Rc::new(pool), &pg_url());
        client
    }

    /// Drop everything the arm created, and clear this thread's driver state.
    async fn teardown(admin: &Client, app_id: &str) {
        probe::reset(app_id);
        zeroship_data_v8::reset_context_for_tests();
        let role = zeroship_core::database_role::per_app_role_name(app_id)
            .expect("transaction fixture app id must produce a valid PostgreSQL role name");
        let _ = admin
            .batch_execute(&format!(
                "DROP SCHEMA IF EXISTS \"{app_id}\" CASCADE; \
                 DROP ROLE IF EXISTS \"{role}\""
            ))
            .await;
    }

    /// Destroy this arm's transaction session even if the arm PANICS.
    ///
    /// Not belt-and-braces. An assertion that fires mid-transaction skips
    /// `teardown`, and the session then stays checked out with an open
    /// transaction for the rest of the binary - holding locks in its schema and
    /// a slot in a pool sized to one. Under mutation that is exactly what
    /// happens, and it turned a mutation of the SAVEPOINT NAMING into a second,
    /// unrelated failure in `transaction_rolls_back_on_async_reject`: a count of
    /// "how many tests did this mutation redden" that includes collateral from
    /// the harness measures the harness, not the code.
    struct SessionGuard(&'static str);

    impl Drop for SessionGuard {
        fn drop(&mut self) {
            probe::reset(self.0);
            zeroship_data_v8::reset_context_for_tests();
        }
    }

    /// **The oracle-sampling trap, as an assertion.**
    ///
    /// A forced cleanup of a POISONED transaction must not withdraw the
    /// connection. Inside a poisoned block `transaction_status()` answers
    /// `None`, because the failed statement's trailing `ReadyForQuery` has not
    /// been consumed, and no retry changes that while the block stays poisoned.
    /// SC-1 defines `None` as indeterminate and indeterminate withdraws, so a
    /// driver that samples the oracle on entry to `Cancelling` destroys a
    /// healthy connection on **every** forced cleanup.
    ///
    /// `driver::cleanup_postgres` therefore issues the cleanup `ROLLBACK` first
    /// and samples after it.
    ///
    /// **Mutation that reddens this arm:** swap the two statements in
    /// `cleanup_postgres` so the `transaction_status()` read precedes the
    /// `batch_execute("ROLLBACK")`. The ack becomes `Indeterminate`, the outcome
    /// becomes `Indeterminate(Cancelled)` instead of `Cancelled(Cancelled)`, the
    /// session reads `Withdrawn`, and the pool loses its only connection.
    #[test]
    fn a_forced_cleanup_on_a_poisoned_block_keeps_a_healthy_connection() {
        const APP: &str = "zs_sc1drv_poisoned";
        block_on(async {
            let admin = provision(APP).await;
            let _session_guard = SessionGuard(APP);

            probe::begin(APP, app_schema(APP), None, probe_backend().await)
                .await
                .expect("BEGIN");
            probe::operation(APP, &format!("CREATE TABLE \"{APP}\".kept (id int)"))
                .await
                .expect("a statement inside the transaction");

            // Poison the block with a real server-side error. A creator callback
            // can swallow exactly this and carry on, which is what makes a
            // forced cleanup of a poisoned transaction an ordinary case.
            let poisoned = probe::operation(APP, "SELECT 1 / 0").await;
            assert!(poisoned.is_err(), "the block must actually be poisoned");
            assert_eq!(
                probe::state(APP),
                Some(TxState::Poisoned),
                "a statement that errored parks the transaction where PostgreSQL \
                 has already put it"
            );
            let pid_before = probe::session_backend_pid(APP).expect("a pinned session");

            // Force it. Cleanup runs from Cancelling, which is the state whose
            // oracle read is the trap.
            let forced = probe::cancel(APP).await;

            assert_eq!(
                forced.outcome,
                Some(TerminalOutcome::Cancelled(CleanupCause::Cancelled)),
                "the cleanup ROLLBACK succeeds from a poisoned block and PROVES \
                 CleanupGoal::OpenTransaction; sampling the oracle before it \
                 reads None, which is indeterminate and withdraws"
            );
            assert_ne!(
                forced.session,
                Some(SessionOwnership::Withdrawn),
                "a poisoned block is a HEALTHY connection once it is rolled back - \
                 withdrawing it destroys a connection on every forced cleanup"
            );
            assert!(
                !probe::withdrawn(APP),
                "no withdrawal tombstone may be set for a proved cleanup"
            );

            // The connection is back in the pool and reusable: the next checkout
            // is the SAME backend. With max_size = 1 there is nothing else it
            // could be handed.
            let (idle, active, total) =
                zeroship_data_v8::pool_counts_for_tests().expect("a pool is installed");
            assert_eq!(
                (idle, active, total),
                (1, 0, 1),
                "a released session returns to the pool as idle"
            );
            probe::begin(APP, app_schema(APP), None, probe_backend().await)
                .await
                .expect("a second BEGIN reuses it");
            assert_eq!(
                probe::session_backend_pid(APP),
                Some(pid_before),
                "the very same physical connection served the next transaction"
            );
            let settled = probe::settle(APP, false).await;
            assert_eq!(settled.outcome, Some(TerminalOutcome::RolledBack));

            teardown(&admin, APP).await;
        });
    }

    /// **`WithdrawSession` genuinely withdraws - and this is the arm that says
    /// what still reaches it now that forced cleanup CANCELS.**
    ///
    /// The disposition SC-1 gives unknown backend health is to destroy the
    /// physical connection rather than return it, and on PostgreSQL that is not
    /// what a drop does: `PoolConnection::drop` calls
    /// `pool.return_client(entry)`, which republishes the lease as idle.
    ///
    /// **The route changed and the name did not, so the route is asserted.**
    /// This arm used to reach withdrawal through "the driver cannot reach the
    /// session, so it cannot roll back" - which is precisely the answer
    /// `cancel_and_reclaim` replaced. What it reaches now is the *best-effort*
    /// half of PostgreSQL cancellation, and it is a case that matters more:
    ///
    /// 1. `HeldSession` takes the session out of the slot with **no statement
    ///    running on it**. The backend is idle inside its transaction block.
    /// 2. Forced cleanup delivers a real `CancelRequest` to that backend, and
    ///    the postmaster confirms it consumed the packet.
    /// 3. **PostgreSQL discards it.** A cancel that arrives while a backend is
    ///    reading its next command clears `QueryCancelPending` and raises
    ///    nothing. The session is not freed, because there was nothing to free.
    /// 4. Nothing returns the session within `probe::cancel_reclaim_grace()`,
    ///    the goal is unproved, and the session is withdrawn.
    ///
    /// That is exactly "what happens when cancellation silently does nothing",
    /// and the answer is: the fallback, unchanged.
    ///
    /// The elapsed-time assertion is what binds the arm to that route. A
    /// cancellation the server acted on frees the session in about a round trip,
    /// so a cleanup that returned quickly took the OTHER path and this arm would
    /// be reporting on cancellation while claiming to report on withdrawal.
    ///
    /// **Mutation that reddens this arm:** make
    /// `context::destroy_tx_connection`'s Postgres arm a plain `drop(client)`
    /// instead of `client.discard()`. The lease returns to the pool and retains
    /// its capacity slot; the next checkout reports the same backend PID the
    /// protocol withdrew.
    #[test]
    fn a_withdrawn_session_never_comes_back_from_the_pool() {
        const APP: &str = "zs_sc1drv_withdraw";
        block_on(async {
            let admin = provision(APP).await;
            let _session_guard = SessionGuard(APP);

            probe::begin(APP, app_schema(APP), None, probe_backend().await)
                .await
                .expect("BEGIN");
            let (_, _, total_before) =
                zeroship_data_v8::pool_counts_for_tests().expect("a pool is installed");
            assert_eq!(total_before, 1, "one connection, checked out");

            // Another future owns the session, and NOTHING IS RUNNING ON IT.
            // A cancel delivered to this backend is discarded by the server, so
            // cleanup cannot free the session and backend health stays unknown -
            // which is the case SC-1 answers with a withdrawal.
            let held = probe::HeldSession::take(APP).expect("hold the session");
            let withdrawn_pid = held.backend_pid().expect("a Postgres session");

            let started = std::time::Instant::now();
            let forced = probe::cancel(APP).await;
            let elapsed = started.elapsed();
            assert_eq!(
                forced.outcome,
                Some(TerminalOutcome::Indeterminate(CleanupCause::Cancelled)),
                "cleanup that could not free the session proves no goal"
            );
            assert!(
                elapsed >= probe::cancel_reclaim_grace(),
                "this arm must reach withdrawal through a cancellation the server \
                 DISCARDED, which is the path that sits out the whole reclaim \
                 grace. Returning in {elapsed:?} means the session came back and \
                 the arm is now measuring cancellation, not withdrawal"
            );
            assert!(
                probe::withdrawn(APP),
                "an indeterminate cleanup withdraws the session"
            );

            // The holder gives it back, exactly as a TxClientSlotGuard's Drop
            // does. THIS is the moment a withdrawal has to survive.
            held.restore();

            let (idle, _, total_after) =
                zeroship_data_v8::pool_counts_for_tests().expect("a pool is installed");
            assert_eq!(
                idle, 0,
                "a withdrawn session must not be published as idle - a plain \
                 drop() would republish it and the next borrower would inherit \
                 the connection the protocol withdrew"
            );
            assert_eq!(
                total_after, 0,
                "the capacity slot is released by eviction, not by redeposit"
            );

            // And the strongest form: whatever the pool opens next is a
            // DIFFERENT backend.
            probe::reset(APP);
            probe::begin(APP, app_schema(APP), None, probe_backend().await)
                .await
                .expect("a fresh BEGIN");
            let fresh_pid = probe::session_backend_pid(APP).expect("a pinned session");
            assert_ne!(
                fresh_pid, withdrawn_pid,
                "the withdrawn backend must be gone; the pool opened a new one"
            );

            // The withdrawn backend is really gone from the server, not merely
            // unreachable from the pool.
            let still_there: i64 = admin
                .query_one(
                    "SELECT count(*) FROM pg_stat_activity WHERE pid = $1",
                    &[&withdrawn_pid],
                )
                .await
                .expect("count the withdrawn backend")
                .get(0);
            assert_eq!(
                still_there, 0,
                "closing the client's request channel terminates the backend; a \
                 session that is still on the server is one a later user could \
                 still be handed"
            );

            let settled = probe::settle(APP, false).await;
            assert_eq!(settled.outcome, Some(TerminalOutcome::RolledBack));
            teardown(&admin, APP).await;
        });
    }

    /// Wait until `pid` is actually executing a statement on the server.
    ///
    /// A `sleep` would be a guess about scheduling; this is the server's own
    /// answer, so the arm that uses it cannot cancel a statement that never
    /// started and then report that cancellation worked.
    async fn wait_until_active(admin: &Client, pid: i32) {
        for _ in 0..200u32 {
            let state: Option<String> = admin
                .query_one("SELECT state FROM pg_stat_activity WHERE pid = $1", &[&pid])
                .await
                .expect("read the backend's state")
                .get(0);
            if state.as_deref() == Some("active") {
                return;
            }
            compio::time::sleep(std::time::Duration::from_millis(10)).await;
        }
        panic!("backend {pid} never reached state=active");
    }

    /// The state PostgreSQL reports for `pid`, or `None` once it is gone.
    async fn backend_state(admin: &Client, pid: i32) -> Option<String> {
        admin
            .query_one("SELECT state FROM pg_stat_activity WHERE pid = $1", &[&pid])
            .await
            .expect("read the backend's state")
            .get(0)
    }

    /// **Forced cleanup CANCELS the running statement; it does not destroy the
    /// connection.**
    ///
    /// This is the common case, not an edge. The execution deadline exists to
    /// bound a slow statement, so it fires *because* a statement is slow - and a
    /// slow statement is one whose future is holding the session out of the
    /// slot. Cleanup could not reach the session, could not roll back, answered
    /// `Indeterminate`, and `Indeterminate` withdraws: the mechanism for
    /// bounding a slow statement responded by killing the backend, every time.
    ///
    /// A `CancelRequest` needs no session. It travels on its own connection and
    /// names the backend by process id, so the canceller captured at install
    /// time reaches a session no one can take out of the slot.
    ///
    /// The arm asserts the whole chain on the server rather than on the
    /// reducer's bookkeeping: the statement really was cancelled (`57014`), the
    /// transaction really was rolled back (the goal `OpenTransaction` is proved,
    /// which only "rolled back" proves), the connection really did come back
    /// (the pool is idle at 1 and the next checkout is the SAME backend pid),
    /// and it happened promptly rather than by sitting out the reclaim grace.
    ///
    /// **Mutation that reddens this arm:** in `driver::cleanup`, answer the
    /// empty-slot `Registry`/`Command` case with `CleanupAck::Indeterminate`
    /// instead of `cancel_and_reclaim(..)` - that is the pre-change behaviour
    /// verbatim. The outcome becomes `Indeterminate(Cancelled)`, the session is
    /// withdrawn, `total_count` drops to 0 and the next checkout is a different
    /// backend.
    #[test]
    fn a_forced_cleanup_cancels_the_running_statement_and_keeps_the_connection() {
        const APP: &str = "zs_sc1drv_cancelrun";
        block_on(async {
            let admin = provision(APP).await;
            let _session_guard = SessionGuard(APP);

            probe::begin(APP, app_schema(APP), None, probe_backend().await)
                .await
                .expect("BEGIN");
            let pid = probe::session_backend_pid(APP).expect("a pinned session");

            // A statement that will not end on its own inside this arm. 60s is
            // deliberately past `DB_STATEMENT_TIMEOUT_MS` (30s) so a failure to
            // cancel shows up as this arm hanging and then failing, never as a
            // server-side timeout that happens to look like a cancellation.
            let running = compio::runtime::spawn(async move {
                probe::operation(APP, "SELECT pg_sleep(60)").await
            });
            wait_until_active(&admin, pid).await;
            assert_eq!(
                probe::session_backend_pid(APP),
                None,
                "the running statement holds the session OUT of the slot - that is \
                 the condition that used to force a withdrawal"
            );

            let started = std::time::Instant::now();
            let forced = probe::cancel(APP).await;
            let elapsed = started.elapsed();

            let statement = running.await.expect("the cancelled statement's task");
            let failure = statement.expect_err("a cancelled statement must not report success");
            assert!(
                format!("{failure:?}").contains("canceling statement due to user request"),
                "the statement must end with PostgreSQL's own 57014, not with some \
                 other failure that would pass this arm for the wrong reason: \
                 {failure:?}"
            );

            assert_eq!(
                forced.outcome,
                Some(TerminalOutcome::Cancelled(CleanupCause::Cancelled)),
                "cancelling frees the session, the cleanup ROLLBACK then PROVES \
                 CleanupGoal::OpenTransaction, and the transaction ends as \
                 cancelled rather than indeterminate"
            );
            assert!(
                !probe::withdrawn(APP),
                "a cancelled statement's connection is healthy once rolled back; \
                 withdrawing it is what this change exists to stop"
            );
            assert!(
                elapsed < probe::cancel_reclaim_grace(),
                "the session must be reclaimed because the cancel landed, not \
                 because the grace expired; {elapsed:?} is the whole grace"
            );

            let (idle, active, total) =
                zeroship_data_v8::pool_counts_for_tests().expect("a pool is installed");
            assert_eq!(
                (idle, active, total),
                (1, 0, 1),
                "the session went back to the pool as idle"
            );
            probe::begin(APP, app_schema(APP), None, probe_backend().await)
                .await
                .expect("a second BEGIN reuses it");
            assert_eq!(
                probe::session_backend_pid(APP),
                Some(pid),
                "the very same physical connection served the next transaction"
            );
            let settled = probe::settle(APP, false).await;
            assert_eq!(settled.outcome, Some(TerminalOutcome::RolledBack));

            teardown(&admin, APP).await;
        });
    }

    /// **A cleanup that outlived its transaction must not roll back the session
    /// it finds in the slot.**
    ///
    /// Cancellation put a second actor in a window that used to have one.
    /// `cleanup` was a straight line with no await between reading the slot and
    /// writing it back, so "the session in the slot" could only be the session
    /// being cleaned up. It now waits for a cancelled statement's holder, and
    /// that wait can outlive the transaction: the `CancellationSql` deadline
    /// fires in its own task, settles `Indeterminate`, withdraws, and releases
    /// the admission - after which the next transaction is admitted, clears the
    /// withdrawal tombstone, and installs ITS session in this very slot. A
    /// resumed cleanup that took whatever it found there would issue `ROLLBACK`
    /// on a healthy, unrelated transaction.
    ///
    /// Two things stop it, and they are not the same thing.
    /// `retire_transaction` wakes the slot waiters, so a cleanup whose
    /// transaction was retired is released immediately rather than sitting out
    /// its grace - that is the first line, and it depends on scheduling.
    /// `CleanupIdentity` is the second, and does not: the cleanup re-reads the
    /// reducer's `(cancellation token, backend generation)` after the wait and
    /// refuses to act on a slot it can no longer prove is its own.
    ///
    /// **The session in the slot belongs to a SUCCESSOR, which is what
    /// production reaches.** A claim is never released while its own session is
    /// parked - `settle_now` emits `WithdrawSession` or `ReleaseSession`
    /// immediately before every `ReleaseAdmission`, and there is no second
    /// emission site - so the only way a stale cleanup finds a filled slot is
    /// that the next caller filled it.
    ///
    /// This used to restore the SAME session and say so, on the grounds that a
    /// genuine successor could not be reached deterministically: admitting one
    /// by the ordinary route requires the claim, releasing the claim is what
    /// wakes this waiter, and the waiter resolves before the successor's `BEGIN`
    /// has run. `probe::abandon_reducer` now re-homes the session onto a
    /// successor lane directly, which reaches the real state without the race.
    /// What the arm rules on is unchanged - a filled slot plus a dead identity -
    /// and so is the observable: **the transaction in that slot is still open on
    /// the server afterwards.**
    ///
    /// **Mutation that reddens this arm:** delete the post-wait
    /// `identity.is_current(..)` check in `driver::cancel_and_reclaim`. The
    /// stale cleanup then takes the client and rolls it back, and
    /// `pg_stat_activity` reports the backend `idle` instead of
    /// `idle in transaction`.
    #[test]
    fn a_cleanup_that_outlived_its_transaction_leaves_the_slot_alone() {
        const APP: &str = "zs_sc1drv_stalecleanup";
        block_on(async {
            let admin = provision(APP).await;
            let _session_guard = SessionGuard(APP);

            probe::begin(APP, app_schema(APP), None, probe_backend().await)
                .await
                .expect("BEGIN");
            let held = probe::HeldSession::take(APP).expect("hold the session");
            let pid = held.backend_pid().expect("a Postgres session");
            assert_eq!(
                backend_state(&admin, pid).await.as_deref(),
                Some("idle in transaction"),
                "precondition: the session is inside its transaction block"
            );

            // Force it. Nothing is running, so the cancel is discarded and the
            // cleanup parks on the slot - which is the state this arm needs.
            let cleanup = compio::runtime::spawn(async move { probe::cancel(APP).await });
            compio::time::sleep(std::time::Duration::from_millis(250)).await;

            // Two steps, with NO await between them, so the woken cleanup task
            // cannot run in the middle: the session comes back, and then the
            // transaction it belonged to is retired out from under the cleanup.
            held.restore();
            probe::abandon_reducer(APP);

            let forced = cleanup.await.expect("the cleanup task");
            assert_eq!(
                forced.outcome, None,
                "the acknowledgement lands on a retired reducer and changes nothing"
            );

            assert_eq!(
                backend_state(&admin, pid).await.as_deref(),
                Some("idle in transaction"),
                "a cleanup that is no longer the current one must not send \
                 ROLLBACK to whatever session it finds in the slot - the \
                 transaction there is still open, and in production it would \
                 belong to the NEXT caller"
            );
            assert_eq!(
                probe::session_backend_pid(APP),
                Some(pid),
                "and the session is still parked, not taken"
            );

            teardown(&admin, APP).await;
        });
    }

    /// **The savepoint names dispatch emits are the reducer's.**
    ///
    /// Two frames opened at the same depth get DIFFERENT names. Under the
    /// depth-derived scheme this replaced they got the same one, and the arm
    /// proves the difference is observable on the server: after the first frame
    /// is released, rolling back to its name must fail with `3B001`. Under a
    /// reused name that rollback would SUCCEED - silently unwinding to the
    /// second frame's scope under the first frame's name.
    ///
    /// **Mutation that reddens this arm:** change `FrameStack::open_child`'s
    /// name to `format!("zs_sp_{}", self.frames.len())`. Both frames are then
    /// `zs_sp_1`, `minted_names()` collapses to one entry, and the
    /// `ROLLBACK TO SAVEPOINT zs_sp_1` at the end succeeds instead of raising
    /// `3B001`.
    #[test]
    fn dispatch_emits_the_reducers_monotonic_savepoint_names() {
        const APP: &str = "zs_sc1drv_names";
        block_on(async {
            let admin = provision(APP).await;
            let _session_guard = SessionGuard(APP);

            probe::begin(APP, app_schema(APP), None, probe_backend().await)
                .await
                .expect("BEGIN");
            probe::operation(APP, &format!("CREATE TABLE \"{APP}\".rows_ (tag text)"))
                .await
                .expect("create table");

            let first = probe::open_frame(APP).await.expect("first frame");
            probe::operation(APP, &format!("INSERT INTO \"{APP}\".rows_ VALUES ('a')"))
                .await
                .expect("write inside the first frame");
            let closed = probe::close_frame(APP, first, true).await;
            assert_eq!(closed.refused, None, "RELEASE must succeed");

            let second = probe::open_frame(APP).await.expect("second frame");
            assert_ne!(first, second, "frame ids are never reused");

            let names = probe::minted_savepoint_names(APP);
            assert_eq!(
                names.len(),
                2,
                "two frames at the SAME depth must mint two distinct names; a \
                 depth-derived scheme mints one name twice and this set collapses \
                 to a single entry. got {names:?}"
            );
            // The names are `zs_sp_<frame sequence>`, NOT `zs_sp_<depth>`, so the
            // first frame's number is whichever is lower - the root frame takes
            // sequence 1, which is why neither of these is `zs_sp_1`.
            let mut ordered = names.clone();
            ordered.sort_by_key(|name| {
                name.rsplit('_')
                    .next()
                    .and_then(|n| n.parse::<u64>().ok())
                    .unwrap_or(u64::MAX)
            });
            let first_name = ordered[0].clone();

            // The decisive server-side check: the first frame's savepoint is
            // GONE, because it was released under its own name and no later
            // frame reused it. A reused name would still be established here and
            // this rollback would SUCCEED, unwinding to the second frame's scope
            // under the first frame's name.
            let shadowed =
                probe::operation(APP, &format!("ROLLBACK TO SAVEPOINT {first_name}")).await;
            let message = shadowed
                .as_ref()
                .err()
                .map(|error| format!("{error:?}"))
                .unwrap_or_default();
            assert!(
                shadowed.is_err(),
                "rolling back to a RELEASED savepoint must be refused; if it \
                 succeeded, a later frame had re-established the same name and \
                 this rollback reached the WRONG scope. names={names:?}"
            );
            assert!(
                message.contains(&format!("savepoint \\\"{first_name}\\\" does not exist"))
                    || message.contains(&format!("savepoint \"{first_name}\" does not exist")),
                "the refusal must be PostgreSQL's 3B001 naming THIS savepoint, not \
                 some other failure that would pass this arm for the wrong reason: \
                 {message}"
            );

            // That statement poisoned the block, so the settle is a rollback.
            let settled = probe::settle(APP, false).await;
            assert_eq!(settled.outcome, Some(TerminalOutcome::RolledBack));
            let _ = second;
            teardown(&admin, APP).await;
        });
    }

    /// **A fired execution deadline in `Preparing`, where no `BEGIN` was ever
    /// sent.**
    ///
    /// The goal `NoTransaction` is fixed only from `Preparing`, and it is proved
    /// by "the session reports no open transaction". There is no session at all
    /// there - `Preparing` is admission plus the authority read, and the client
    /// is acquired by `IssueBegin` - so the driver must prove the goal from the
    /// protocol's own session ownership rather than from an empty slot, and must
    /// send no SQL.
    ///
    /// The transaction still settles, still releases its admission claim, and
    /// still does NOT withdraw: nothing was opened, so there is nothing whose
    /// health is unknown.
    ///
    /// **Mutation that reddens this arm:** in `driver::cleanup`, answer the
    /// empty-slot case with `CleanupAck::Indeterminate` unconditionally (delete
    /// the `SessionOwnership::None` arm). The goal is then unproved, the outcome
    /// becomes `Indeterminate` instead of `Cancelled`, and the tombstone is set
    /// for a session that never existed - which would destroy the NEXT
    /// transaction's connection, because `put_tx_client_for` consults it.
    #[test]
    fn a_deadline_that_fires_in_preparing_settles_without_a_begin() {
        const APP: &str = "zs_sc1drv_preparing";
        block_on(async {
            let admin = provision(APP).await;
            let _session_guard = SessionGuard(APP);

            // Admit, and stop there. `admit_in_preparing_for_tests` performs the
            // admission half of `begin_top_level` and returns before the
            // authority observation that would issue BEGIN.
            probe::admit_only(APP);
            assert_eq!(
                probe::state(APP),
                Some(TxState::Preparing),
                "no BEGIN has been sent"
            );
            assert_eq!(
                probe::session(APP),
                Some(SessionOwnership::None),
                "Preparing holds no session: the client is acquired by IssueBegin"
            );
            let (idle_before, _, _) =
                zeroship_data_v8::pool_counts_for_tests().expect("a pool is installed");

            let fired = probe::fire_execution_deadline(APP).await;

            assert_eq!(
                fired.outcome,
                Some(TerminalOutcome::Cancelled(CleanupCause::DeadlineExpired(
                    zeroship_data_orm::transaction::reducer::deadline::DeadlineKind::Execution
                ))),
                "the goal NoTransaction is proved by construction - BEGIN was \
                 never sent, so nothing can be open"
            );
            assert!(
                !probe::withdrawn(APP),
                "there is no session to withdraw, and setting the tombstone here \
                 would destroy the NEXT transaction's connection"
            );
            assert!(
                probe::state(APP).is_none(),
                "ReleaseAdmission retires the transaction on every path to Settled"
            );

            let (idle_after, active_after, _) =
                zeroship_data_v8::pool_counts_for_tests().expect("a pool is installed");
            assert_eq!(
                (idle_after, active_after),
                (idle_before, 0),
                "a cleanup in Preparing touches no connection at all"
            );

            // The claim was released, so the next transaction is admitted
            // rather than parked forever.
            probe::begin(APP, app_schema(APP), None, probe_backend().await)
                .await
                .expect("the admission claim was released");
            let settled = probe::settle(APP, false).await;
            assert_eq!(settled.outcome, Some(TerminalOutcome::RolledBack));

            teardown(&admin, APP).await;
        });
    }
}
