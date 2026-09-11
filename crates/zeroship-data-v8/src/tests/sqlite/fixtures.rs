//! SQLite fixtures used by this test module.

use crate::tests::fixtures::parity;

use zeroship_data_sql::compile::raw_column_name;

/// Drive a future to completion on a fresh compio runtime. The
/// integration target has no global runtime — each `#[test]` builds
/// its own so tests stay isolated.
pub(super) fn run<F: std::future::Future>(f: F) -> F::Output {
    compio::runtime::Runtime::new()
        .expect("compio runtime build")
        .block_on(f)
}

/// Supply a project key for the apps explicitly named by this fixture.
pub(super) fn with_project_key(
    app_ids: &[&str],
    hex: &str,
) -> crate::tests::fixtures::SuppliedProjectKeysGuard {
    crate::tests::fixtures::supply_project_key(app_ids, hex)
}

pub(super) const SQLITE_RUNTIME_RPC_SHIM: &str = r#"
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
        if (err && err.details !== undefined) body.details = err.details;
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

/// `email` unique + plaintext, `ssn` randomised-encrypted with a `last4` mask.
pub(super) fn users_encrypted_ssn_schema() -> zeroship_data_sql::value::Value {
    zeroship_data_sql::value!({
        "email": {"type": "string", "required": true, "unique": true},
        "name": {"type": "string", "required": true},
        "ssn": {
            "type": "string",
            "encrypted": true,
            "mask": {"kind": "last4", "classification": "spi"}
        }
    })
}

/// The three system indexes every confined table carries.
pub(super) fn system_indexes_sqlite(app_id: &str, collection: &str) -> String {
    format!(
        r#"
CREATE INDEX IF NOT EXISTS "{app_id}"."{collection}_deleted_at_idx" ON "{collection}" ("deleted_at");
CREATE INDEX IF NOT EXISTS "{app_id}"."{collection}_updated_at_idx" ON "{collection}" ("updated_at");
CREATE INDEX IF NOT EXISTS "{app_id}"."{collection}_created_by_idx" ON "{collection}" ("created_by");
"#
    )
}

/// Raw DDL matching [`users_encrypted_ssn_schema`].
///
/// Post-storage-flip layout: the field's own column (`ssn`) holds the
/// masked representation as bare `TEXT`; the sibling raw column (named via
/// [`raw_column_name`], NOT spelled out here) carries the declared type,
/// the encryption sentinel, and any constraints.
pub(super) fn users_encrypted_ssn_ddl() -> String {
    let raw_ssn = raw_column_name("ssn");
    format!(
        r#"CREATE TABLE IF NOT EXISTS "default"."users" ({SYSTEM_COLUMNS_SQLITE},
  "email" TEXT NOT NULL,
  "name" TEXT NOT NULL,
  "{raw_ssn}" BLOB /* zero-migrate:enc:string */,
  "ssn" TEXT /* zero-migrate:mask:kind=last4,classification=spi */
);
{}
CREATE UNIQUE INDEX IF NOT EXISTS "default"."users_email_key" ON "users" ("email");
"#,
        system_indexes_sqlite("default", "users")
    )
}

/// Create the `users` table in the dev app file BEFORE the runtime boots.
///
/// `dir` is the same directory `parity::sqlite_url` points the runtime at, so the
/// fixture writes `<dir>/zs-default.sqlite` - the exact file the data plane will
/// ATTACH. `default` is the app id the runtime derives with no `APP_ID` in the
/// env snapshot.
pub(super) fn apply_schema_ahead_of_runtime(dir: &tempfile::TempDir, ddl: &str) {
    crate::tests::fixtures::tables::create_sqlite_table(dir.path(), "default", ddl);
}

pub(super) struct SqliteRuntimeSource {
    pub(super) source: String,
    pub(super) descriptor: String,
}

pub(super) fn sqlite_runtime_source(
    collection: &str,
    schema: &zeroship_data_sql::value::Value,
    body: &str,
) -> SqliteRuntimeSource {
    let source = format!(
        r#"
import {{ env }} from "zeroship";

const COLLECTION = "{collection}";

{body}
"#
    ) + SQLITE_RUNTIME_RPC_SHIM;
    SqliteRuntimeSource {
        source,
        descriptor: parity::runtime_descriptor(collection, schema),
    }
}

pub(super) fn dispatch_sqlite_runtime(
    dir: &tempfile::TempDir,
    source: &SqliteRuntimeSource,
    name: &str,
) -> zeroship_data_sql::value::Value {
    let url = parity::sqlite_url(dir);
    let (status, body) = parity::dispatch_zs_with_descriptor(
        &url,
        &source.source,
        name,
        parity::DEV_APP_ID,
        &source.descriptor,
    );
    assert_eq!(status, 200, "{name} failed: {body}");
    body
}

pub(super) fn assert_write_path_fast_path(label: &str) {
    let counters = crate::tests::fixtures::recording::id_probes();
    assert_eq!(
        counters.len(),
        0,
        "{label}: plain write must not resolve row ids: {counters:?}",
    );
    assert_eq!(
        counters.len(),
        0,
        "{label}: plain write must not run an upsert conflict probe: {counters:?}",
    );
}

/// The seven system columns and the `["id"]` primary key, SQLite spelling.
pub(super) const SYSTEM_COLUMNS_SQLITE: &str = r#"
  id TEXT PRIMARY KEY,
  created_at TEXT NOT NULL DEFAULT CURRENT_TIMESTAMP,
  updated_at TEXT NOT NULL DEFAULT CURRENT_TIMESTAMP,
  created_by TEXT NULL,
  updated_by TEXT NULL,
  version INTEGER NOT NULL DEFAULT 1,
  deleted_at TEXT NULL"#;
