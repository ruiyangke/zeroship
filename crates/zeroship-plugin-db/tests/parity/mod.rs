use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;
use std::time::Duration;

use serde_json::{json, Value};
use zeroship_plugin_db::service::{DbService, DbServiceConfig};
use zeroship_runtime::channel::CancelFlag;
use zeroship_runtime::plugin::NativePlugin;
use zeroship_runtime::runtime::Runtime;
use zeroship_runtime::{init_v8, EnvSnapshot, FetchOutcome, ModuleEntry, RequestCtx, SettledFetch};

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
    /// Measured on this matrix, not inferred: `setup` returned 200 and `seed`
    /// died on the 15s `pending timeout` with `pg_stat_activity` showing two
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
/// `APP_ID` (`crates/runtime/src/core/plugin.rs`), which is what `dispatch_zs`
/// boots with. It is also the dev app id, so the matrix runs on the same
/// `<db_dir>/zs-default.sqlite` a `pnpm dev` app does.
const MATRIX_APP_ID: &str = "default";

/// The declared shape `matrix_source`'s `setup` registers. Kept beside the JS so
/// the two cannot drift: the pre-apply below must create exactly the columns the
/// procedures then read, and `registerModel` no longer reconciles them.
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
/// `registerModel` applies no DDL on either dialect since the 2026-08-10
/// cutover, so a migration process has to have run first - on the dev tier the
/// vite dev-server's apply-ahead, on Postgres the `migrated` service at deploy.
/// Without this the `setup` dispatch still returns 200 (it registers metadata)
/// and every later dispatch fails with `no such table`, which is exactly how
/// these three tests broke.
fn apply_matrix_schema_ahead_of_runtime(url: &str, collection: &str) {
    let Some(path) = url.strip_prefix("sqlite:") else {
        // The Postgres leg. Same engine, same confined ceiling, same declared
        // shape - only the dialect and the driver differ, which is the whole
        // point of a parity matrix. See `apply_matrix_schema_ahead_of_postgres`.
        apply_matrix_schema_ahead_of_postgres(url, collection);
        return;
    };
    let db_dir = std::path::Path::new(path)
        .parent()
        .expect("the sqlite parity url names a file inside a directory")
        .to_path_buf();
    crate::support::tables::create_sqlite_table(&db_dir, MATRIX_APP_ID, &matrix_ddl_sqlite(collection));
}

/// Raw SQLite DDL for [`matrix_schema`].
///
/// Hand-written, not rendered: plugin-db does not own DDL, and a matrix whose
/// fixture came out of the layer under test could not detect that layer being
/// wrong (see `support::tables`). The PostgreSQL twin is
/// [`matrix_ddl_postgres`]; the two must describe the SAME declared shape in
/// each dialect's spelling, because that equivalence IS what this matrix
/// asserts.
fn matrix_ddl_sqlite(collection: &str) -> String {
    format!(
        r#"CREATE TABLE IF NOT EXISTS "{MATRIX_APP_ID}"."{collection}" (
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
CREATE INDEX IF NOT EXISTS "{MATRIX_APP_ID}"."{collection}_deleted_at_idx" ON "{collection}" ("deleted_at");
CREATE INDEX IF NOT EXISTS "{MATRIX_APP_ID}"."{collection}_updated_at_idx" ON "{collection}" ("updated_at");
CREATE INDEX IF NOT EXISTS "{MATRIX_APP_ID}"."{collection}_created_by_idx" ON "{collection}" ("created_by");
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
fn matrix_ddl_postgres(collection: &str) -> String {
    format!(
        r#"CREATE TABLE IF NOT EXISTS "{MATRIX_APP_ID}"."{collection}" (
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
CREATE INDEX IF NOT EXISTS "{collection}_deleted_at_idx" ON "{MATRIX_APP_ID}"."{collection}" ("deleted_at");
CREATE INDEX IF NOT EXISTS "{collection}_updated_at_idx" ON "{MATRIX_APP_ID}"."{collection}" ("updated_at");
CREATE INDEX IF NOT EXISTS "{collection}_created_by_idx" ON "{MATRIX_APP_ID}"."{collection}" ("created_by");
"#
    )
}

// THE CONFINED CEILING AND ITS BINDER ARE GONE FROM THIS FILE.
//
// They existed to hand an `EffectivePolicy` to the migration engine, which this
// matrix used to drive on both legs. plugin-db no longer depends on the engine
// in any profile, so there is nothing here to hand a policy to: both legs build
// their table from `zeroship-schema` now.
//
// That drops one consumer of the platform `[[inject]]` system-shape fragment
// under `policies/`, and `tests/inject_policy_mirror_gate.sh` counts consumers
// on purpose, so its EXPECTED_RUST_CONSUMERS is lowered in the same commit with
// this as the reason. (The fragment's filename is deliberately NOT spelled here:
// the gate finds consumers by grepping files that hold both `include_str!` and
// that exact path, so a comment naming it would become a phantom consumer the
// day this file gains an unrelated `include_str!`.)
// The gate's own guidance is to do exactly that for a deliberate deletion; what
// it must never absorb silently is a consumer that stopped taking the fragment
// while still injecting.
//
// WHAT THIS MATRIX STOPPED PROVING. Both legs no longer take the seven system
// columns from a policy document, so this file can no longer be read as evidence
// that a creator's deployed table and the dev table agree. The fragment's own
// header (see that same fragment) already said `zeroship-schema` is a producer
// it cannot reach and that
// the two "differ on purpose" (varchar(255) vs text on id / created_by /
// updated_by), so that reading was already narrower than it looked. What the
// matrix still proves is what its assertions actually compare: one declared
// shape, two dialects, identical JSON projection.

/// Create the matrix collection's table on POSTGRES before the runtime boots.
///
/// # Why this builds the table instead of driving the engine
///
/// It used to drive `zeroship-migrate` under plugin-db's confined ceiling, on
/// the argument that the system columns then arrived from the same policy
/// document on both legs. plugin-db no longer depends on the engine in any
/// profile, so that option is gone, and the argument was weaker than it read:
/// `policies/confined-system-shape.inject.toml` says outright that
/// `zeroship-schema` is a producer it CANNOT reach and that the two "already
/// differ on purpose". Both legs now take the same `zeroship-schema` emitter,
/// which is what the matrix actually needs - one shape, two dialects.
///
/// # What this reproduces, and what it does NOT
///
/// Same caveat as the SQLite helper, in the same direction, plus one more.
/// `crates/zeroship-migrated` replays AUTHORED migration-IR envelopes; this
/// runs a rendered CREATE. So a defect in envelope lowering, in journal
/// versioning, or in the recorder is invisible on both legs. AND, since the
/// engine left: this table's `id` / `created_by` / `updated_by` are `text`,
/// where a deployed creator's are `varchar(255)`. What the matrix still pins is
/// everything downstream of the applied table - types, defaults, ordering,
/// transaction nesting and the JSON projection `env.db` hands back - and it
/// pins them identically on both dialects, which is its purpose.
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
fn apply_matrix_schema_ahead_of_postgres(url: &str, collection: &str) {
    // project_schema == app_id: plugin-db's PG data plane resolves a collection
    // to `"<app_id>"."<collection>"` (the PG data plane's own qualification),
    // and this DDL qualifies into that same schema, so the runtime reads the
    // table this created rather than a different one.
    let ddl = matrix_ddl_postgres(collection);

    block_on(async move {
        let pool = compio_postgres::Pool::connect(url, 2)
            .await
            .expect("admin pool for the parity schema");

        pool.batch_execute(&format!(
            "DROP SCHEMA IF EXISTS \"{MATRIX_APP_ID}\" CASCADE; \
             CREATE SCHEMA \"{MATRIX_APP_ID}\""
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
        // afterthought: `crates/zeroship-migrated` grants exactly this
        // (`GRANT USAGE ON SCHEMA ... TO <role>` + table/sequence privileges,
        // `apply.rs`) immediately after its own apply, for the same reason. Here
        // the equivalent step is plugin-db's own `ensure_per_app_role`, which
        // must run AFTER the table exists because it grants `ON ALL TABLES IN
        // SCHEMA`.
        zeroship_plugin_db::auth::bootstrap::ensure_per_app_role(&pool, MATRIX_APP_ID)
            .await
            .expect("provision the matrix app's runtime role");
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

const __plat = (typeof globalThis.__zsDbPlatform === "function")
    ? globalThis.__zsDbPlatform(env.db)
    : undefined;
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

async function setup(_input, _ctx) {
    await __plat.registerModel(COLLECTION, {
        title: { type: "string", required: true },
        flag: { type: "boolean", required: true },
        meta: { type: "object", required: true },
        optional: { type: "string" },
        rank: { type: "int", required: true },
        occurred_at: { type: "date" },
        payload_bytes: { type: "bytes" },
        payload_json: { type: "json" },
    });
    return { ok: true };
}
setup.config = { kind: "action" };

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
    await coll.insert({
        title: "typed-roundtrip",
        flag: true,
        meta: { kind: "typed" },
        optional: "typed",
        rank: 41,
        occurred_at: new Date(TYPED_DATE_ISO),
        payload_bytes: TYPED_BYTES_B64,
        payload_json: {
            nested: { ok: true },
            items: [1, "two", false],
            nullish: null,
        },
    });
    const rows = await coll.find(
        { title: "typed-roundtrip", flag: 1 },
        { limit: 1 },
    );
    return rows[0] ? projectTypedRow(rows[0]) : null;
}
typedRoundTrip.config = { kind: "action" };

const _procedures = { setup, seed, transactionMatrix, typedRoundTrip };
"#
    .replace("__COLLECTION__", collection)
    .replace("__TYPED_DATE_ISO__", TYPED_DATE_ISO)
    .replace("__TYPED_BYTES_B64__", &typed_bytes_b64())
        + SHIM
}

pub fn dispatch_zs(url: &str, source: &str, name: &str) -> (u16, Value) {
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
            block_on(async {
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
    let body = String::from_utf8_lossy(&body).into_owned();
    let json: Value =
        serde_json::from_str(&body).unwrap_or_else(|_| Value::String(body.clone()));
    (status, json)
}

pub fn run_matrix(url: &str) -> MatrixSnapshot {
    // Keep each matrix run isolated even when the backend URL is reused
    // across tests or legs.
    let collection = format!(
        "fixtures_parity_{}",
        MATRIX_COUNTER.fetch_add(1, Ordering::Relaxed)
    );
    let source = matrix_source(&collection);

    apply_matrix_schema_ahead_of_runtime(url, &collection);

    let (status, body) = dispatch_zs(url, &source, "setup");
    assert_eq!(status, 200, "setup failed: {body}");

    let (status, body) = dispatch_zs(url, &source, "seed");
    assert_eq!(status, 200, "seed failed: {body}");
    let seed = extract_json(&body);

    let (status, body) = dispatch_zs(url, &source, "transactionMatrix");
    assert_eq!(status, 200, "transactionMatrix failed: {body}");
    let tx = extract_json(&body);

    let (status, body) = dispatch_zs(url, &source, "typedRoundTrip");
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
    })
}

