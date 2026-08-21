use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;
use std::time::Duration;

use serde_json::{json, Value};
use zeroship_plugin_db::DbPlugin;
use zeroship_runtime::channel::CancelFlag;
use zeroship_runtime::plugin::NativePlugin;
use zeroship_runtime::runtime::Runtime;
use zeroship_runtime::{init_v8, EnvSnapshot, FetchOutcome, ModuleEntry, RequestCtx, SettledFetch};

pub struct MatrixSnapshot {
    pub seed: Value,
    pub tx: Value,
    pub typed: Value,
}

static MATRIX_COUNTER: AtomicU64 = AtomicU64::new(0);

thread_local! {
    /// ONE compio runtime per test thread, alive for the whole thread.
    ///
    /// This harness used to build a fresh `compio::runtime::Runtime` inside each
    /// `dispatch_zs` and drop it on the way out, which is invisible on SQLite and
    /// fatal on Postgres. plugin-db parks its `compio_postgres::Pool` in a
    /// THREAD-LOCAL `IsolateDbContext` that outlives any one dispatch, and
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
    let backend = zeroship_plugin_db::backend::SqliteBackend::new(db_dir)
        .expect("open a SQLite backend on the matrix db_dir");
    block_on(async {
        zeroship_plugin_db::register_model::apply_declared_schema_to_dev_sqlite_for_tests(
            &backend,
            MATRIX_APP_ID,
            collection,
            &matrix_schema(),
            &json!([]),
        )
        .await
        .expect("apply the matrix schema ahead of the runtime")
    });
}

/// The confined table-shape ceiling plugin-db's own SQLite arm compiles in.
///
/// Assembled from THE SAME TWO FILES the SQLite arm compiles in rather than
/// restated here on purpose: the two legs of this matrix must inject the
/// identical seven system columns, `["id"]` PK and three system indexes, or the
/// projections they produce differ for a reason that has nothing to do with the
/// dialect. The grants come from plugin-db's own file; the `[[inject]]` rule is
/// the platform-wide fragment every consumer takes.
/// `tests/inject_policy_mirror_gate.sh` counts those consumers and refuses if one
/// stops taking it - dropping the second `include_str!` here would leave this
/// matrix comparing two dialects that both inject nothing.
const CONFINED_CEILING_TOML: &str = concat!(
    include_str!("../../policies/confined.policy.toml"),
    include_str!("../../../../policies/confined-system-shape.inject.toml"),
);

/// Bind that ceiling to the matrix app's schema, the way the Postgres path does.
///
/// `plugin-db/policies/confined.policy.toml` carries NO `schema.cross_schema`
/// grant, and on SQLite it does not need one - the dialect has no schemas, so the
/// guard's cross-schema gate is inert and the SQLite arm composes the file as
/// authored. On Postgres it is load-bearing: `GuardConfig` takes schema authority
/// from the effective policy and NOTHING else ("Schema authority comes only from
/// the explicit effective policy", `zero-migrate-guard/src/guard/mod.rs`), so the
/// unbound ceiling denies the engine's own `CREATE TABLE "default"."<coll>"` with
/// `CrossSchema { schema: "default" }`. Measured: that is exactly how this leg
/// failed before this grant was appended.
///
/// This mirrors `bind_confined_charter_to_schema`
/// (`crates/migrated/src/policy.rs`), which the managed PG server runs over the
/// SAME ceiling shape before composing it. IN ONE RESPECT IT IS WEAKER: the
/// production binder also NARROWS the authored `schema.create_table` and
/// `schema.rename` grants from `scope = "all"` to this one schema. Skipping that
/// leaves the test's policy strictly LOOSER than production's, so this matrix
/// cannot be read as evidence that the confined binding confines anything - it is
/// a schema applier for a projection test, not a proof about the guard. What it
/// does have to get right is the table SHAPE, and that comes from the `[[inject]]`
/// rule, which is the file's verbatim.
///
/// It also cannot silently stop applying: `effective_policy_from_charter_toml`
/// refuses an unknown key, and a ceiling that ever grows its own cross-schema
/// grant would make this a duplicate rather than a no-op.
fn matrix_effective_policy() -> zero_migrate::EffectivePolicy {
    let charter = format!(
        "{CONFINED_CEILING_TOML}\n\
         [[grant]]\n\
         key = \"schema.cross_schema\"\n\
         value = true\n\
         scope = {{ include = [\"{MATRIX_APP_ID}\"] }}\n"
    );
    zero_migrate::effective_policy_from_charter_toml(&charter)
        .expect("plugin-db's confined ceiling composes once bound to the matrix app schema")
}

/// Create the matrix collection's table on POSTGRES before the runtime boots.
///
/// # Why this is not a smaller `CREATE TABLE`
///
/// The PG arm of `registerModel` has been a pure no-op since long before the
/// SQLite one became one: on Postgres the schema authority is `crates/migrated`,
/// which applies ahead of the worker at deploy. A hand-written `CREATE TABLE`
/// here would test the projection of a table shape no creator ever gets. Driving
/// the engine, under plugin-db's own confined ceiling, is what makes the two
/// legs comparable - the system columns, their DDL defaults, the PK and the
/// three system indexes all arrive from the same policy document on both.
///
/// # What this reproduces, and what it does NOT
///
/// Same caveat as the SQLite helper, in the same direction. `crates/migrated`
/// replays AUTHORED migration-IR envelopes; this plans a declarative diff of the
/// declared schema against live introspection. So a defect in envelope lowering,
/// in journal versioning of authored migrations, or in the recorder is invisible
/// on both legs of this matrix. What it does pin is everything downstream of the
/// applied table: types, defaults, ordering, transaction nesting and the JSON
/// projection `env.db` hands back.
///
/// # Why it drops its two schemas first
///
/// The SQLite leg gets a fresh `tempfile::tempdir` per run; Postgres does not.
/// `run_matrix` mints its collection name from a per-PROCESS counter, so a second
/// run of this test reuses `fixtures_parity_1` and finds the previous run's rows
/// already in it - measured, as a `seed` projection with every row DUPLICATED
/// against a single-copy SQLite side. Dropping the app schema and its journal
/// schema makes the leg repeatable and, unlike a unique-name-per-run scheme,
/// leaves nothing behind on a shared server. It is the same pair of statements
/// `crates/zeroship-migrate-adapter/tests/smoke_apply_pg.rs` opens with, and it
/// touches only the two schemas this function itself creates.
fn apply_matrix_schema_ahead_of_postgres(url: &str, collection: &str) {
    use zero_migrate::apply::backend::MigrationBackend;
    use zero_migrate::driver::SqlSession;
    use zero_migrate::{
        desired_snapshot_for_dialect, Approval, DeclarativeAuthor, ExecutorConfig, GuardConfig,
        MigrationEngine, PostgresBackend, SqlDialect,
    };
    use zeroship_migrate_adapter::CompioPgSession;

    let descriptor = zeroship_plugin_db::register_model::collection_descriptor_for_tests(
        MATRIX_APP_ID,
        collection,
        &matrix_schema(),
        &json!([]),
    )
    .expect("the matrix schema translates to an engine descriptor");

    block_on(async move {
            let session = CompioPgSession::connect(url)
                .await
                .expect("connect the migration session to the parity database");

            // ONE policy, not two. `plan_declarative` and `apply_declarative` both
            // OVERWRITE the config's policy with this argument
            // (`engine.rs`: `cfg.clone().with_effective_policy(effective.clone())`
            // and `policy_exec_cfg.effective = effective.clone()`), so a guard
            // composed separately would be discarded before it decided anything.
            let effective = matrix_effective_policy();
            // project_schema == app_id: plugin-db's PG data plane resolves a
            // collection to `"<app_id>"."<collection>"`
            // (`backend/postgres.rs::build_ensure_app_schema`), so the engine has
            // to own that same schema or the runtime would read a different table
            // from the one this applied.
            let exec_cfg =
                ExecutorConfig::new(MATRIX_APP_ID, MATRIX_APP_ID, effective.clone());
            session
                .batch(&format!(
                    "DROP SCHEMA IF EXISTS \"{}\" CASCADE; \
                     DROP SCHEMA IF EXISTS \"{}\" CASCADE; \
                     CREATE SCHEMA \"{}\"",
                    exec_cfg.project_schema, exec_cfg.pg.meta_schema, exec_cfg.project_schema
                ))
                .await
                .expect("reset the parity app schema and its migration journal");

            let desired = desired_snapshot_for_dialect(
                MATRIX_APP_ID,
                std::slice::from_ref(&descriptor),
                SqlDialect::Postgres,
                &effective,
            )
            .expect("desired snapshot for the matrix collection");

            let engine = MigrationEngine::new();
            let backend = PostgresBackend::new_generic(&session);
            // Introspected rather than assumed empty, even though the drop above
            // guarantees it is: a real applier diffs against what the server
            // actually holds, and an assumed-empty `live` would author a CREATE
            // over an existing table on the day someone removes the drop.
            let live = backend
                .snapshot_schema(&exec_cfg)
                .await
                .expect("introspect the live parity schema");
            let live_ownership: std::collections::HashMap<String, String> = live
                .tables
                .keys()
                .map(|t| (t.clone(), MATRIX_APP_ID.to_string()))
                .collect();

            let author = DeclarativeAuthor::new_for_dialect(
                MATRIX_APP_ID,
                MATRIX_APP_ID,
                SqlDialect::Postgres,
            );
            let guard_cfg = GuardConfig::confined_with_effective(MATRIX_APP_ID, effective.clone())
                .for_dialect(SqlDialect::Postgres);
            let plan = engine
                .plan_declarative(
                    &desired,
                    &live,
                    &live_ownership,
                    &author,
                    &[],
                    &guard_cfg,
                    &effective,
                )
                .expect("plan the matrix collection against live Postgres");

            engine
                .apply_declarative(
                    &plan,
                    &effective,
                    Approval::Approved,
                    &backend,
                    &exec_cfg,
                    "parity-matrix",
                )
                .await
                .expect("apply the matrix schema ahead of the runtime");

            // The engine creates the table as the ADMIN role. plugin-db's PG data
            // plane then reads it as the per-app role, which at this point has no
            // USAGE on the schema - measured, as `permission denied for schema
            // default` in the server log, surfacing to the caller as a bare
            // `500 internal error`.
            //
            // Provisioning that role is part of the deploy-time apply, not an
            // afterthought: `crates/migrated` grants exactly this
            // (`GRANT USAGE ON SCHEMA ... TO <role>` + table/sequence privileges,
            // `apply.rs`) immediately after its own apply, for the same reason.
            // Here the equivalent step is plugin-db's own `ensure_per_app_role`,
            // which must run AFTER the table exists because it grants `ON ALL
            // TABLES IN SCHEMA`.
            let pool = compio_postgres::Pool::connect(url, 2)
                .await
                .expect("admin pool for the parity role provisioning");
            zeroship_plugin_db::auth::ensure_admin_schema(&pool)
                .await
                .expect("ensure the platform admin schema");
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
const TYPED_BYTES_B64: &str = "3q2+7w==";

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
    .replace("__TYPED_BYTES_B64__", TYPED_BYTES_B64)
        + SHIM
}

pub fn dispatch_zs(url: &str, source: &str, name: &str) -> (u16, Value) {
    init_v8();
    let modules = vec![ModuleEntry {
        specifier: "index.js".into(),
        source: source.into(),
    }];
    let plugins: Vec<Arc<dyn NativePlugin>> = vec![Arc::new(DbPlugin::new(
        url.to_string(),
        None,
        "parity-test-worker",
    ))];
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

    MatrixSnapshot { seed, tx, typed }
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
        "payload_bytes": TYPED_BYTES_B64,
        "payload_json": {
            "nested": { "ok": true },
            "items": [1, "two", false],
            "nullish": null
        }
    })
}
