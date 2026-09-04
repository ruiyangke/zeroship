use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Duration;

use serde_json::{Value, json};
use zeroship_plugin_db::service::{DbService, DbServiceConfig};
use zeroship_runtime::channel::CancelFlag;
use zeroship_runtime::plugin::NativePlugin;
use zeroship_runtime::runtime::Runtime;
use zeroship_runtime::{EnvSnapshot, FetchOutcome, ModuleEntry, RequestCtx, SettledFetch, init_v8};

pub struct MatrixSnapshot {
    pub seed: Value,
    pub tx: Value,
    pub typed: Value,
    /// The table this run wrote to. Carried out so a caller can go BEHIND
    /// `env.db` and read the stored cells with a direct query - the SDK cannot
    /// be its own witness for what reached the disk.
    pub collection: String,
}

static MATRIX_COUNTER: AtomicU64 = AtomicU64::new(0);

thread_local! {
    /// ONE compio runtime per test thread, alive for the whole thread.
    ///
    /// This harness used to build a fresh `compio::runtime::Runtime` inside each
    /// `dispatch_zs` and drop it on the way out, which is invisible on SQLite and
    /// fatal on Postgres. plugin-db parks its `compio_postgres::Pool` in a
    /// THREAD-LOCAL `ThreadDbContext` that outlives any one dispatch, and
    /// `DbPlugin::register` only clears it when the DB URL changes. So the second
    /// PG dispatch submits a pooled query on sockets registered with an io_uring
    /// that no longer exists, and it never completes.
    ///
    /// Measured on this matrix, not inferred: `seed` died on the 15s `pending
    /// timeout` with `pg_stat_activity` showing two
    /// connections sitting `idle`/`ClientRead` for the whole window - the runtime
    /// never issued the INSERT. `crates/plugin-db/tests/native_transaction.rs`
    /// hit the identical wall and carries the same thread-local; its header is
    /// the long-form account.
    ///
    /// Production has one compio runtime per worker thread for the process
    /// lifetime, so this is the faithful shape as well as the working one.
    static RT: compio::runtime::Runtime =
        compio::runtime::Runtime::new().expect("build the per-thread compio runtime");
}

/// Drive `fut` on this thread's long-lived compio runtime.
fn block_on<F: std::future::Future>(fut: F) -> F::Output {
    RT.with(|rt| rt.block_on(fut))
}

pub fn sqlite_url(root: &tempfile::TempDir) -> String {
    format!("sqlite:{}", root.path().join("parity.sqlite").display())
}

/// The app id the runtime derives when `EnvSnapshot::empty()` carries no
/// `APP_ID` (`crates/runtime/src/core/plugin.rs`). It is the DEV app id, so a
/// SQLite matrix keyed on it runs against the same `<db_dir>/zs-default.sqlite`
/// a `pnpm dev` app does - which is the property that leg is for.
///
/// The Postgres leg must NOT use it. `apply_matrix_schema_ahead_of_postgres`
/// opens with `DROP SCHEMA ... CASCADE`, and a shared server has exactly one
/// `default` schema, so two Postgres matrix runs on one server destroy each
/// other's table. That is what they did until 2026-09-04. Postgres callers pass
/// their own per-test app id; the SQLite leg keeps this one, and its per-run
/// `tempfile::tempdir` is what isolates it.
pub const DEV_APP_ID: &str = "default";

/// The matrix's deployed field shape. Kept beside the JS so the pre-apply, the
/// runtime descriptor, and the procedures cannot drift.
fn matrix_schema() -> Value {
    json!({
        "_meta": {"strictness": "lenient"},
        "title": {"type": "string", "required": true},
        "flag": {"type": "boolean", "required": true},
        "meta": {"type": "object", "required": true},
        "optional": {"type": "string"},
        "rank": {"type": "int", "required": true},
        "occurred_at": {"type": "date"},
        "payload_bytes": {"type": "bytes"},
        "payload_json": {"type": "json"}
    })
}

/// Create the matrix collection's table BEFORE the runtime boots.
///
/// A migration process must have run first - on the dev tier the Vite
/// dev-server's apply-ahead, on Postgres the migration service at deploy.
/// Without this every dispatch fails with a missing-table error.
fn apply_matrix_schema_ahead_of_runtime(url: &str, app_id: &str, collection: &str) {
    let Some(path) = url.strip_prefix("sqlite:") else {
        // The Postgres leg. Same engine, same confined ceiling, same declared
        // shape - only the dialect and the driver differ, which is the whole
        // point of a parity matrix. See `apply_matrix_schema_ahead_of_postgres`.
        apply_matrix_schema_ahead_of_postgres(url, app_id, collection);
        return;
    };
    let db_dir = std::path::Path::new(path)
        .parent()
        .expect("the sqlite parity url names a file inside a directory")
        .to_path_buf();
    crate::support::tables::create_sqlite_table(
        &db_dir,
        app_id,
        &matrix_ddl_sqlite(app_id, collection),
    );
}

/// Raw SQLite DDL for [`matrix_schema`].
///
/// Hand-written, not rendered: plugin-db does not own DDL, and a matrix whose
/// fixture came out of the layer under test could not detect that layer being
/// wrong (see `support::tables`). The PostgreSQL twin is
/// [`matrix_ddl_postgres`]; the two must describe the SAME declared shape in
/// each dialect's spelling, because that equivalence IS what this matrix
/// asserts.
fn matrix_ddl_sqlite(app_id: &str, collection: &str) -> String {
    format!(
        r#"CREATE TABLE IF NOT EXISTS "{app_id}"."{collection}" (
  id TEXT PRIMARY KEY,
  created_at TEXT NOT NULL DEFAULT CURRENT_TIMESTAMP,
  updated_at TEXT NOT NULL DEFAULT CURRENT_TIMESTAMP,
  created_by TEXT NULL,
  updated_by TEXT NULL,
  version INTEGER NOT NULL DEFAULT 1,
  deleted_at TEXT NULL,
  "title" TEXT NOT NULL,
  "flag" INTEGER NOT NULL,
  "meta" TEXT NOT NULL DEFAULT '{{}}',
  "optional" TEXT,
  "rank" INTEGER NOT NULL,
  "occurred_at" TEXT,
  "payload_bytes" TEXT,
  "payload_json" TEXT DEFAULT '{{}}'
);
CREATE INDEX IF NOT EXISTS "{app_id}"."{collection}_deleted_at_idx" ON "{collection}" ("deleted_at");
CREATE INDEX IF NOT EXISTS "{app_id}"."{collection}_updated_at_idx" ON "{collection}" ("updated_at");
CREATE INDEX IF NOT EXISTS "{app_id}"."{collection}_created_by_idx" ON "{collection}" ("created_by");
"#
    )
}

/// Raw PostgreSQL DDL for [`matrix_schema`]. Twin of [`matrix_ddl_sqlite`].
///
/// The dialect differences here are the ones the matrix exists to hold constant
/// downstream: `TIMESTAMPTZ`/`NOW()` for SQLite's `TEXT`/`CURRENT_TIMESTAMP`,
/// `BOOLEAN` for `INTEGER`, `JSONB` for `TEXT`. Note the index targets flip -
/// PostgreSQL qualifies the TABLE, SQLite qualifies the INDEX NAME, because on
/// SQLite the app file is an ATTACHed database rather than a schema.
fn matrix_ddl_postgres(app_id: &str, collection: &str) -> String {
    format!(
        r#"CREATE TABLE IF NOT EXISTS "{app_id}"."{collection}" (
  id TEXT PRIMARY KEY,
  created_at TIMESTAMPTZ NOT NULL DEFAULT NOW(),
  updated_at TIMESTAMPTZ NOT NULL DEFAULT NOW(),
  created_by TEXT NULL,
  updated_by TEXT NULL,
  version INTEGER NOT NULL DEFAULT 1,
  deleted_at TIMESTAMPTZ NULL,
  "title" TEXT NOT NULL,
  "flag" BOOLEAN NOT NULL,
  "meta" JSONB NOT NULL DEFAULT '{{}}'::jsonb,
  "optional" TEXT,
  "rank" INTEGER NOT NULL,
  "occurred_at" TIMESTAMPTZ,
  "payload_bytes" BYTEA,
  "payload_json" JSONB DEFAULT '{{}}'::jsonb
);
CREATE INDEX IF NOT EXISTS "{collection}_deleted_at_idx" ON "{app_id}"."{collection}" ("deleted_at");
CREATE INDEX IF NOT EXISTS "{collection}_updated_at_idx" ON "{app_id}"."{collection}" ("updated_at");
CREATE INDEX IF NOT EXISTS "{collection}_created_by_idx" ON "{app_id}"."{collection}" ("created_by");
"#
    )
}

/// Create the matrix collection's table on POSTGRES before the runtime boots.
///
/// Like the SQLite leg, this fixture uses a matching hand-authored table. The
/// matrix proves runtime behavior downstream of schema application; it does not
/// cover migration recording, lowering, policy, or journal behavior.
///
/// # Why it drops the app schema first
///
/// The SQLite leg gets a fresh `tempfile::tempdir` per run; Postgres does not.
/// `run_matrix` mints its collection name from a per-PROCESS counter, so a
/// second run reuses `fixtures_parity_1` and finds the previous run's rows
/// already in it - measured, as a `seed` projection with every row DUPLICATED
/// against a single-copy SQLite side. Dropping the app schema makes the leg
/// repeatable without leaving a unique-name-per-run trail on a shared server.
/// It no longer drops a journal schema: nothing here writes one now.
fn apply_matrix_schema_ahead_of_postgres(url: &str, app_id: &str, collection: &str) {
    // project_schema == app_id: plugin-db's PG data plane resolves a collection
    // to `"<app_id>"."<collection>"` (the PG data plane's own qualification),
    // and this DDL qualifies into that same schema, so the runtime reads the
    // table this created rather than a different one.
    let ddl = matrix_ddl_postgres(app_id, collection);

    block_on(async move {
        let pool = compio_postgres::Pool::connect(url, 2)
            .await
            .expect("admin pool for the parity schema");

        pool.batch_execute(&format!(
            "DROP SCHEMA IF EXISTS \"{app_id}\" CASCADE; \
             CREATE SCHEMA \"{app_id}\""
        ))
        .await
        .expect("reset the parity app schema");

        pool.batch_execute(&ddl)
            .await
            .unwrap_or_else(|e| panic!("matrix DDL failed: {e}\n{ddl}"));

        // The table is created as the ADMIN role. plugin-db's PG data plane then
        // reads it as the per-app role, which at this point has no USAGE on the
        // schema - measured, as `permission denied for schema default` in the
        // server log, surfacing to the caller as a bare `500 internal error`.
        //
        // Provisioning that role is part of the deploy-time apply, not an
        // afterthought. The role recipe supplies schema and sequence reach; the
        // binding supplies explicit column grants.
        zeroship_plugin_db::auth::bootstrap::ensure_per_app_role(&pool, app_id)
            .await
            .expect("provision the matrix app's runtime role");
        super::support::grant_all_runtime_table_columns(&pool, app_id, collection).await;
    });
}

// `maybe_pg_url` lived here and is DELETED. It answered "is there a Postgres?"
// with `Option`, and its one caller turned `None` into an early `return` - a test
// that reports "ok" for work it did not do, which is the exact failure
// `integration.rs::require_pg` was rewritten to stop making. The Postgres parity
// leg now calls `require_pg` like its 108 siblings and fails without a database.

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

const TYPED_DATE_ISO: &str = "2026-05-24T12:34:56.789Z";
const TYPED_DATE_MS: i64 = 1_779_626_096_789;

/// The bytes the `t.bytes()` round trip must preserve, stated AS BYTES.
///
/// This is the only pinned fact about the byte column, and everything else is
/// derived from it: the base64 the matrix writes is `typed_bytes_b64()`, the
/// base64 it must read back is the same string, and the cell psql must show on
/// disk is these four bytes. It is deliberately not a copied output - the
/// constant that used to sit here was the literal `"3q2+7w=="` and its sibling
/// in `integration.rs` was `"M3EyKzd3PT0="`, the base64 OF that base64, pinned
/// because that is what the code returned. A pin taken from the code under test
/// cannot fail when the code is wrong.
pub const TYPED_BYTES_RAW: [u8; 4] = [0xDE, 0xAD, 0xBE, 0xEF];

/// The wire form of [`TYPED_BYTES_RAW`]: `t.bytes()` is exchanged with JS as a
/// base64 string (`sdks/db/src/types.ts`), so this is what a caller passes in
/// and what a correct round trip hands back.
pub fn typed_bytes_b64() -> String {
    use base64::Engine as _;
    base64::engine::general_purpose::STANDARD.encode(TYPED_BYTES_RAW)
}

pub fn matrix_source(collection: &str) -> String {
    r#"
import { env } from "zeroship";

const COLLECTION = "__COLLECTION__";
const TYPED_DATE_ISO = "__TYPED_DATE_ISO__";
const TYPED_BYTES_B64 = "__TYPED_BYTES_B64__";

function projectRows(rows) {
    return rows.map((row) => ({
        title: row.title,
        flag: row.flag,
        meta: row.meta,
        optional: row.optional ?? null,
        rank: row.rank,
        version: row.version,
        deleted_at: row.deleted_at ?? null,
        id_kind: typeof row.id,
        created_at_kind: typeof row.created_at,
        updated_at_kind: typeof row.updated_at,
    }));
}

function projectTypedRow(row) {
    return {
        title: row.title,
        flag: row.flag,
        occurred_at: row.occurred_at,
        occurred_at_kind: typeof row.occurred_at,
        payload_bytes: row.payload_bytes,
        payload_json: row.payload_json,
    };
}

async function seed(_input, _ctx) {
    const coll = env.db.collection(COLLECTION);
    await coll.insert({
        title: "null-last",
        flag: true,
        meta: { idx: 1 },
        optional: null,
        rank: 2,
    });
    await coll.insert({
        title: "alpha",
        flag: false,
        meta: { idx: 2 },
        optional: "aaa",
        rank: 1,
    });
    await coll.insert({
        title: "bravo",
        flag: true,
        meta: { idx: 3 },
        optional: "bbb",
        rank: 3,
    });
    const rows = await coll.find({}, { orderBy: { optional: 1 } });
    return projectRows(rows);
}
seed.config = { kind: "action" };

async function transactionMatrix(_input, _ctx) {
    try {
        await env.db.transaction(async (tx) => {
            await tx[COLLECTION].insert({
                title: "outer-doomed",
                flag: false,
                meta: { phase: "rollback" },
                optional: "txn-z",
                rank: 9,
            });
            throw new Error("rollback outer");
        });
    } catch (_) {}

    await env.db.transaction(async (tx) => {
        await tx[COLLECTION].insert({
            title: "outer",
            flag: true,
            meta: { phase: "outer" },
            optional: "txn-a",
            rank: 10,
        });
        try {
            await env.db.transaction(async (tx2) => {
                await tx2[COLLECTION].insert({
                    title: "inner-doomed",
                    flag: false,
                    meta: { phase: "inner" },
                    optional: "txn-b",
                    rank: 11,
                });
                throw new Error("rollback inner");
            });
        } catch (_) {}
        await tx[COLLECTION].insert({
            title: "outer-2",
            flag: true,
            meta: { phase: "outer-2" },
            optional: "txn-c",
            rank: 12,
        });
    });

    const coll = env.db.collection(COLLECTION);
    const rows = await coll.find(
        { optional: { $in: ["txn-a", "txn-b", "txn-c", "txn-z"] } },
        { orderBy: { rank: 1 } },
    );
    return projectRows(rows);
}
transactionMatrix.config = { kind: "action" };

async function typedRoundTrip(_input, _ctx) {
    const coll = env.db.collection(COLLECTION);
    const payloadJson = {
        nested: { ok: true },
        items: [1, "two", false],
        nullish: null,
    };
    await coll.insert({
        title: "typed-roundtrip",
        flag: true,
        meta: { kind: "typed" },
        optional: "typed",
        rank: 41,
        occurred_at: new Date(TYPED_DATE_ISO),
        payload_bytes: TYPED_BYTES_B64,
        payload_json: payloadJson,
    });
    const sourceRows = await coll.find(
        { title: "typed-roundtrip", flag: 1 },
        { limit: 1 },
    );
    const source = sourceRows[0];
    if (!source) return null;

    await coll.insert({
        title: "typed-roundtrip-echo",
        flag: true,
        meta: { kind: "typed-echo" },
        optional: "typed-echo",
        rank: 42,
        occurred_at: source.occurred_at,
        payload_bytes: TYPED_BYTES_B64,
        payload_json: payloadJson,
    });
    const echoRows = await coll.find(
        {
            title: "typed-roundtrip-echo",
            occurred_at: { $gte: source.occurred_at },
        },
        { limit: 1 },
    );
    return {
        source: projectTypedRow(source),
        echo: echoRows[0] ? projectTypedRow(echoRows[0]) : null,
    };
}
typedRoundTrip.config = { kind: "action" };

const _procedures = { seed, transactionMatrix, typedRoundTrip };
"#
    .replace("__COLLECTION__", collection)
    .replace("__TYPED_DATE_ISO__", TYPED_DATE_ISO)
    .replace("__TYPED_BYTES_B64__", &typed_bytes_b64())
        + SHIM
}

pub fn runtime_descriptor(collection: &str, schema: &Value) -> String {
    let mut fields = schema.clone();
    let strictness = fields
        .as_object_mut()
        .and_then(|map| map.remove("_meta"))
        .and_then(|meta| {
            meta.get("strictness")
                .and_then(Value::as_str)
                .map(str::to_owned)
        })
        .unwrap_or_else(|| "strict".to_string());
    serde_json::to_string(&json!({
        "version": 2,
        "collections": {
            (collection): {
                "fields": fields,
                "options": {
                    "softDelete": false,
                    "versioning": false,
                    "strictness": strictness,
                },
                "indexes": [],
            },
        },
    }))
    .expect("runtime descriptor serializes")
}

/// Drive one RPC through a real runtime against `url`, as app `app_id`.
///
/// `app_id` is INJECTED, not inherited. It used to be absent, so the runtime
/// fell back to `default` (`crates/zeroship-runtime/src/core/plugin.rs`) and
/// every Postgres matrix run addressed the same schema. Passing it explicitly
/// also stops the fixture from depending on that fallback continuing to exist.
pub fn dispatch_zs_with_descriptor(
    url: &str,
    source: &str,
    name: &str,
    app_id: &str,
    descriptor: &str,
) -> (u16, Value) {
    init_v8();
    let modules = vec![ModuleEntry {
        specifier: "index.js".into(),
        source: source.into(),
    }];
    let plugins: Vec<Arc<dyn NativePlugin>> = vec![
        DbService::new(DbServiceConfig {
            url: url.to_string(),
            worker_id: "parity-test-worker".to_string(),
            meter: None,
        })
        .expect("db service")
        .plugin(),
    ];
    let env_vars = std::collections::HashMap::from([("APP_ID".to_string(), app_id.to_string())]);
    let runtime = Runtime::builder()
        .modules(modules)
        .env_vars(env_vars)
        .plugins(plugins)
        .runtime_descriptor(Some(descriptor.to_string()))
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
        FetchOutcome::Pending { rx, cancel: _ } => block_on(async {
            runtime.start_pump();
            let settled = compio::time::timeout(Duration::from_secs(15), rx.recv())
                .await
                .expect("pending timeout")
                .expect("settled error");
            match settled {
                SettledFetch::Response { status, body, .. } => (status, body),
                _ => panic!("expected Response variant"),
            }
        }),
        _ => panic!("unexpected outcome variant"),
    };
    let body = String::from_utf8_lossy(&body).into_owned();
    let json: Value = serde_json::from_str(&body).unwrap_or_else(|_| Value::String(body.clone()));
    (status, json)
}

/// Run the whole matrix against `url` as `app_id`.
///
/// `app_id` is the caller's, not a constant. On PostgreSQL it names the schema
/// this run drops and rebuilds, so a shared server needs it to be per-test;
/// SQLite callers pass [`DEV_APP_ID`] on purpose (see its note).
pub fn run_matrix(url: &str, app_id: &str) -> MatrixSnapshot {
    // Keep each matrix run isolated even when the backend URL is reused
    // across tests or legs.
    let collection = format!(
        "fixtures_parity_{}",
        MATRIX_COUNTER.fetch_add(1, Ordering::Relaxed)
    );
    let source = matrix_source(&collection);
    let descriptor = runtime_descriptor(&collection, &matrix_schema());

    apply_matrix_schema_ahead_of_runtime(url, app_id, &collection);

    let (status, body) = dispatch_zs_with_descriptor(url, &source, "seed", app_id, &descriptor);
    assert_eq!(status, 200, "seed failed: {body}");
    let seed = extract_json(&body);

    let (status, body) =
        dispatch_zs_with_descriptor(url, &source, "transactionMatrix", app_id, &descriptor);
    assert_eq!(status, 200, "transactionMatrix failed: {body}");
    let tx = extract_json(&body);

    let (status, body) =
        dispatch_zs_with_descriptor(url, &source, "typedRoundTrip", app_id, &descriptor);
    assert_eq!(status, 200, "typedRoundTrip failed: {body}");
    let typed = extract_json(&body);

    MatrixSnapshot {
        seed,
        tx,
        typed,
        collection,
    }
}

pub fn extract_json(body: &Value) -> Value {
    body.get("json").cloned().unwrap_or(Value::Null)
}

pub fn expected_seed_projection() -> Value {
    json!([
        {
            "title": "alpha",
            "flag": false,
            "meta": { "idx": 2 },
            "optional": "aaa",
            "rank": 1,
            "version": 1,
            "deleted_at": null,
            "id_kind": "string",
            "created_at_kind": "number",
            "updated_at_kind": "number"
        },
        {
            "title": "bravo",
            "flag": true,
            "meta": { "idx": 3 },
            "optional": "bbb",
            "rank": 3,
            "version": 1,
            "deleted_at": null,
            "id_kind": "string",
            "created_at_kind": "number",
            "updated_at_kind": "number"
        },
        {
            "title": "null-last",
            "flag": true,
            "meta": { "idx": 1 },
            "optional": null,
            "rank": 2,
            "version": 1,
            "deleted_at": null,
            "id_kind": "string",
            "created_at_kind": "number",
            "updated_at_kind": "number"
        }
    ])
}

pub fn expected_tx_projection() -> Value {
    json!([
        {
            "title": "outer",
            "flag": true,
            "meta": { "phase": "outer" },
            "optional": "txn-a",
            "rank": 10,
            "version": 1,
            "deleted_at": null,
            "id_kind": "string",
            "created_at_kind": "number",
            "updated_at_kind": "number"
        },
        {
            "title": "outer-2",
            "flag": true,
            "meta": { "phase": "outer-2" },
            "optional": "txn-c",
            "rank": 12,
            "version": 1,
            "deleted_at": null,
            "id_kind": "string",
            "created_at_kind": "number",
            "updated_at_kind": "number"
        }
    ])
}

pub fn expected_typed_projection() -> Value {
    json!({
        "source": {
            "title": "typed-roundtrip",
            "flag": true,
            "occurred_at": TYPED_DATE_MS,
            "occurred_at_kind": "number",
            "payload_bytes": typed_bytes_b64(),
            "payload_json": {
                "nested": { "ok": true },
                "items": [1, "two", false],
                "nullish": null
            }
        },
        "echo": {
            "title": "typed-roundtrip-echo",
            "flag": true,
            "occurred_at": TYPED_DATE_MS,
            "occurred_at_kind": "number",
            "payload_bytes": typed_bytes_b64(),
            "payload_json": {
                "nested": { "ok": true },
                "items": [1, "two", false],
                "nullish": null
            }
        }
    })
}
