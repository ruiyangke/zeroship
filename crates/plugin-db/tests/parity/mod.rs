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

pub fn sqlite_url(root: &tempfile::TempDir) -> String {
    format!("sqlite:{}", root.path().join("parity.sqlite").display())
}

#[allow(
    dead_code,
    reason = "the Postgres parity target uses this helper; sqlite_integration compiles the shared module without calling it"
)]
pub async fn maybe_pg_url() -> Option<String> {
    let url = std::env::var("PG_TEST_URL")
        .unwrap_or_else(|_| "postgres://postgres:test@localhost:5434/postgres".to_string());
    match compio_postgres::connect(&url, compio_postgres::NoTls).await {
        Ok((client, connection)) => {
            compio::runtime::spawn(async move {
                let _ = connection.run().await;
            })
            .detach();
            drop(client);
            Some(url)
        }
        Err(_) => None,
    }
}

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
