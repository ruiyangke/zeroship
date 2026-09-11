//! Adapter contracts exercised through JavaScript dispatch.

use crate::parity;

#[allow(unused_imports)]
use crate::schema_fixture::{fixture_table_sql, fixture_table_sql_for};

#[allow(unused_imports)]
use zeroship_migrate::schema::query::FkEmission;

use zeroship_data_sql::compile::raw_column_name;

/// Drive a future to completion on a fresh compio runtime. The
/// integration target has no global runtime — each `#[test]` builds
/// its own so tests stay isolated.
fn run<F: std::future::Future>(f: F) -> F::Output {
    compio::runtime::Runtime::new()
        .expect("compio runtime build")
        .block_on(f)
}

#[test]
fn parity_matrix_sqlite_seed_projection_matches_contract() {
    run(async {
        let dir = tempfile::tempdir().expect("create parity dir");
        let snapshot = parity::run_matrix(&parity::sqlite_url(&dir), parity::DEV_APP_ID);
        assert_eq!(snapshot.seed, parity::expected_seed_projection());
    });
}

#[test]
fn parity_matrix_sqlite_transaction_projection_matches_contract() {
    run(async {
        let dir = tempfile::tempdir().expect("create parity dir");
        let snapshot = parity::run_matrix(&parity::sqlite_url(&dir), parity::DEV_APP_ID);
        assert_eq!(snapshot.tx, parity::expected_tx_projection());
    });
}

#[test]
fn parity_matrix_sqlite_typed_projection_matches_contract() {
    run(async {
        let dir = tempfile::tempdir().expect("create parity dir");
        let snapshot = parity::run_matrix(&parity::sqlite_url(&dir), parity::DEV_APP_ID);
        assert_eq!(snapshot.typed, parity::expected_typed_projection());
    });
}

/// The dev tier must store a `t.bytes()` value as a BLOB of the caller's bytes.
///
/// WHY THIS IS SEPARATE FROM THE PROJECTION TEST ABOVE, and why SQLite needed a
/// test at all. Before `crud::bytes_pass`, the projection test above was GREEN
/// while the stored cell was wrong: rusqlite bound the SDK's base64 string as
/// TEXT into a BLOB-affinity column, read it back as TEXT, and
/// `read_pipeline::normalize_bytes_value` passes a string through untouched - so
/// the input reappeared and the round trip looked perfect. `typeof()` is what
/// separates the two, and it is the reason the SQLite leg is the control that
/// isolates the layer: the SDK, the read pipeline and the JSON wire shape are
/// shared with Postgres, so a defect visible on one and hidden on the other has
/// to live below them, in the bind.
#[test]
fn bytes_column_stores_a_raw_blob_on_sqlite() {
    run(async {
        let dir = tempfile::tempdir().expect("create parity dir");
        let snapshot = parity::run_matrix(&parity::sqlite_url(&dir), parity::DEV_APP_ID);

        // A fresh backend has attached nothing: the matrix's app database is a
        // separate file (`<dir>/zs-default.sqlite`) reached through an ATTACH
        // alias, so re-attach it before the schema-qualified name resolves.
        let client = crate::live_tests::sqlite::Inspector::open(dir.path());
        // `query` materialises every cell as `Option<String>` and renders a BLOB
        // as `<N bytes blob>`, so ask SQLite itself for the discriminant and the
        // hex - the same route `p5_*` uses for ciphertext.
        let sql = format!(
            "SELECT typeof(payload_bytes), hex(payload_bytes) FROM \"default\".\"{}\" \
             WHERE title = 'typed-roundtrip'",
            snapshot.collection
        );
        let rows = client.query(&sql, &[]).await.expect("SELECT");
        assert_eq!(rows.len(), 1, "the typed round-trip row must exist");

        let kind = rows[0][0].clone().expect("typeof() is never null");
        let hex = rows[0][1].clone().expect("hex() is never null");
        let expected_hex: String = parity::TYPED_BYTES_RAW
            .iter()
            .map(|b| format!("{b:02X}"))
            .collect();

        assert_eq!(
            kind, "blob",
            "a t.bytes() cell must be a BLOB, not {kind}. 'text' here is the \
             write path binding the base64 wire string as text into a \
             BLOB-affinity column, which round-trips through env.db while \
             storing the wrong thing"
        );
        assert_eq!(hex, expected_hex, "the BLOB must hold the caller's bytes");
    });
}

/// Supply a project key for the apps explicitly named by this fixture.
fn with_project_key(
    app_ids: &[&str],
    hex: &str,
) -> zeroship_data_v8::testing::SuppliedProjectKeysGuard {
    zeroship_data_v8::testing::supply_project_key_for_tests(app_ids, hex)
}

const SQLITE_RUNTIME_RPC_SHIM: &str = r#"
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
fn users_encrypted_ssn_schema() -> zeroship_data_sql::value::Value {
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

/// One randomised-encrypted column and no mask - the fast-path fixtures assert
/// a PLAIN write skips row resolution, so the encrypted column must exist but
/// stay untouched by the write under test.
fn users_encrypted_secret_schema() -> zeroship_data_sql::value::Value {
    zeroship_data_sql::value!({
        "email": {"type": "string", "required": true, "unique": true},
        "name": {"type": "string", "required": true},
        "secret": {
            "type": "string",
            "encrypted": true
        }
    })
}

/// The three system indexes every confined table carries.
fn system_indexes_sqlite(app_id: &str, collection: &str) -> String {
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
fn users_encrypted_ssn_ddl() -> String {
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

/// Raw DDL matching [`users_encrypted_secret_schema`].
fn users_encrypted_secret_ddl() -> String {
    format!(
        r#"CREATE TABLE IF NOT EXISTS "default"."users" ({SYSTEM_COLUMNS_SQLITE},
  "email" TEXT NOT NULL,
  "name" TEXT NOT NULL,
  "secret" BLOB /* zero-migrate:enc:string */
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
fn apply_schema_ahead_of_runtime(dir: &tempfile::TempDir, ddl: &str) {
    crate::support::tables::create_sqlite_table(dir.path(), "default", ddl);
}

struct SqliteRuntimeSource {
    source: String,
    descriptor: String,
}

fn sqlite_runtime_source(
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

fn dispatch_sqlite_runtime(
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

fn assert_write_path_fast_path(label: &str) {
    let counters = crate::live_tests::recording::id_probes();
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

#[test]
fn upsert_insert_branch_auto_mints_id_sqlite_runtime() {
    let _keys = with_project_key(&["default"], &"e".repeat(64));

    run(async {
        let dir = tempfile::tempdir().expect("tempdir");
        let schema = users_encrypted_ssn_schema();
        apply_schema_ahead_of_runtime(&dir, &users_encrypted_ssn_ddl());
        let source = sqlite_runtime_source(
            "users",
            &schema,
            r#"
async function upsertInsert(_input, _ctx) {
    return await env.db.collection(COLLECTION).upsert(
        {
            email: "mint@example.com",
            name: "Mint",
            ssn: "123-45-6789"
        },
        { conflictFields: ["email"] },
    );
}
upsertInsert.config = { kind: "action" };

const _procedures = { upsertInsert };
"#,
        );

        let result = dispatch_sqlite_runtime(&dir, &source, "upsertInsert");
        let row = parity::extract_json(&result);
        let id = row
            .get("id")
            .and_then(|v| v.as_str())
            .expect("upsert insert branch must return minted id");
        assert!(
            id.starts_with("user_"),
            "auto-minted id should use collection-derived prefix: {row}"
        );
        assert_eq!(
            row.get("version").and_then(|v| v.as_i64()),
            Some(1),
            "freshly inserted upsert row should start at version 1: {row}"
        );

        let client = crate::live_tests::sqlite::Inspector::open(dir.path());
        let rows = client
            .query(
                r#"SELECT id, version FROM "default"."users" WHERE email = 'mint@example.com'"#,
                &[],
            )
            .await
            .expect("SELECT runtime upsert row");
        assert_eq!(rows.len(), 1, "exactly one runtime-upsert row");
        assert_eq!(
            rows[0][0].as_deref(),
            Some(id),
            "stored row keeps minted id"
        );
        assert_eq!(
            rows[0][1].as_deref(),
            Some("1"),
            "stored row version defaults to 1"
        );
    });
}

#[test]
fn upsert_conflict_update_preserves_insert_only_fields_and_encrypts_sqlite_runtime() {
    let _keys = with_project_key(&["default"], &"f".repeat(64));

    run(async {
        use rusqlite::types::Value as TypedCell;
        use zeroship_data_orm::encryption;

        let dir = tempfile::tempdir().expect("tempdir");
        let schema = users_encrypted_ssn_schema();
        apply_schema_ahead_of_runtime(&dir, &users_encrypted_ssn_ddl());
        let source = sqlite_runtime_source(
            "users",
            &schema,
            r#"
async function upsertConflict(_input, _ctx) {
    const coll = env.db.collection(COLLECTION);
    const first = await coll.upsert(
        {
            email: "alice@example.com",
            name: "Alice",
            created_by: "usr_seed",
            ssn: "123-45-6789"
        },
        { conflictFields: ["email"] },
    );
    const second = await coll.upsert(
        {
            email: "alice@example.com",
            name: "Alice Updated",
            created_by: "usr_new",
            updated_by: "usr_update",
            ssn: "987-65-4321"
        },
        { conflictFields: ["email"] },
    );
    return { first, second };
}
upsertConflict.config = { kind: "action" };

const _procedures = { upsertConflict };
"#,
        );

        let result = dispatch_sqlite_runtime(&dir, &source, "upsertConflict");
        let payload = parity::extract_json(&result);
        let first = payload.get("first").expect("first response row");
        let second = payload.get("second").expect("second response row");
        let first_id = first
            .get("id")
            .and_then(|v| v.as_str())
            .expect("generated id");
        assert!(first_id.starts_with("user_"));
        // The conflicting insert mints a candidate identity; the update must
        // preserve the stored identity and use it for encryption's AAD.
        assert_eq!(
            second.get("id").and_then(|v| v.as_str()),
            Some(first_id),
            "conflict update must keep the original id"
        );
        // The two documents supply DIFFERENT actor ids, and neither lands.
        // `created_by` / `updated_by` are charter-assigned, so the value comes
        // from the request's authenticated user - here there is none, and the
        // generator yields NULL rather than the id the document asked for.
        //
        // This asserted `usr_seed` / `usr_update` until the write pass started
        // iterating the charter. It was pinning the DB-3 shape: app JS naming
        // whichever actor it liked on a row it wrote.
        assert!(
            first
                .get("created_by")
                .is_none_or(zeroship_data_sql::value::Value::is_null),
            "a supplied created_by must not land on the insert arm: {first:?}"
        );
        assert!(
            second
                .get("created_by")
                .is_none_or(zeroship_data_sql::value::Value::is_null),
            "a supplied created_by must not land on the conflict arm: {second:?}"
        );
        assert!(
            second
                .get("updated_by")
                .is_none_or(zeroship_data_sql::value::Value::is_null),
            "a supplied updated_by must not land on the conflict arm: {second:?}"
        );
        assert_eq!(
            second.get("version").and_then(|v| v.as_i64()),
            Some(2),
            "conflict update must auto-bump version"
        );

        let client = crate::live_tests::sqlite::Inspector::open(dir.path());
        let raw_ssn = raw_column_name("ssn");
        let typed = client
            .query_typed(
                &format!(
                    r#"SELECT id, created_by, updated_by, version, "{raw_ssn}", ssn
                   FROM "default"."users"
                   WHERE email = 'alice@example.com'"#
                ),
                &[],
            )
            .await
            .expect("SELECT typed conflict row");
        assert_eq!(typed.rows.len(), 1, "exactly one row after conflict upsert");
        let row = &typed.rows[0];

        match &row[0] {
            TypedCell::Text(id) => assert_eq!(id, first_id),
            other => panic!("id must be TEXT, got {other:?}"),
        }
        // Read back from the DATABASE, not from the returned row, that neither
        // supplied actor id was stored. Both documents named one; there is no
        // authenticated user on this vector, so the stored value is NULL.
        assert!(
            matches!(&row[1], TypedCell::Null),
            "created_by must be NULL when no actor is bound, got {:?}",
            row[1]
        );
        assert!(
            matches!(&row[2], TypedCell::Null),
            "updated_by must be NULL when no actor is bound, got {:?}",
            row[2]
        );
        match &row[3] {
            TypedCell::Integer(version) => assert_eq!(*version, 2),
            other => panic!("version must be INTEGER, got {other:?}"),
        }
        let stored_blob = match &row[4] {
            TypedCell::Blob(bytes) => bytes.clone(),
            other => panic!("{raw_ssn} must be stored as BLOB ciphertext, got {other:?}"),
        };
        match &row[5] {
            TypedCell::Text(masked) => assert_eq!(masked, "***-**-4321"),
            other => panic!("ssn (the masked column) must be TEXT, got {other:?}"),
        }
        assert_ne!(
            stored_blob,
            b"987-65-4321".to_vec(),
            "conflict-updated raw storage must not equal plaintext"
        );
        // The field's own column (`ssn`) already asserted equal to the mask
        // above; pin the negative directly too - it must never be the
        // plaintext the conflict-update wrote.
        match &row[5] {
            TypedCell::Text(masked) => assert_ne!(
                masked, "987-65-4321",
                "ssn (field's own column) must not hold plaintext"
            ),
            other => panic!("ssn (the masked column) must be TEXT, got {other:?}"),
        }

        let keys =
            zeroship_data_orm::encryption::KeyStore::new(crate::testing::isolate_key_source());
        let key = keys.resolve("default").await.expect("resolve key");
        let plaintext = zeroship_data_orm::encryption::aead::decrypt(
            &key,
            &stored_blob,
            &encryption::canonical_aad("default", "users", "ssn", first_id.as_bytes()),
        )
        .expect("decrypt stored conflict ciphertext");
        assert_eq!(
            plaintext,
            b"987-65-4321".to_vec(),
            "stored ciphertext must decrypt to the updated plaintext"
        );
    });
}

#[test]
fn update_non_id_filter_keeps_randomised_ciphertext_readable_sqlite_runtime() {
    let _keys = with_project_key(&["default"], &"7".repeat(64));

    run(async {
        use rusqlite::types::Value as TypedCell;
        use zeroship_data_orm::encryption;

        let dir = tempfile::tempdir().expect("tempdir");
        let schema = users_encrypted_ssn_schema();
        apply_schema_ahead_of_runtime(&dir, &users_encrypted_ssn_ddl());
        let source = sqlite_runtime_source(
            "users",
            &schema,
            r#"
async function seed(_input, _ctx) {
    return await env.db.collection(COLLECTION).upsert(
        {
            email: "alice@example.com",
            name: "Alice",
            ssn: "123-45-6789"
        },
        { conflictFields: ["email"] },
    );
}
seed.config = { kind: "action" };

async function updateByEmail(_input, _ctx) {
    return await env.db.collection(COLLECTION).update(
        { email: "alice@example.com" },
        { ssn: "987-65-4321" },
    );
}
updateByEmail.config = { kind: "action" };

const _procedures = { seed, updateByEmail };
"#,
        );

        let seeded = dispatch_sqlite_runtime(&dir, &source, "seed");
        let seed_row = parity::extract_json(&seeded);
        let seed_id = seed_row["id"].as_str().expect("generated id");
        let updated = dispatch_sqlite_runtime(&dir, &source, "updateByEmail");
        let row = parity::extract_json(&updated);
        assert_eq!(
            row.get("id").and_then(|v| v.as_str()),
            Some(seed_id),
            "update by non-id filter should still target the seeded row"
        );

        let client = crate::live_tests::sqlite::Inspector::open(dir.path());
        let raw_ssn = raw_column_name("ssn");
        let typed = client
            .query_typed(
                &format!(
                    r#"SELECT id, "{raw_ssn}", ssn
                   FROM "default"."users"
                   WHERE email = 'alice@example.com'"#
                ),
                &[],
            )
            .await
            .expect("SELECT typed updated row");
        assert_eq!(typed.rows.len(), 1, "exactly one updated row");

        let row = &typed.rows[0];
        let row_id = match &row[0] {
            TypedCell::Text(id) => id.clone(),
            other => panic!("id must be TEXT, got {other:?}"),
        };
        let stored_blob = match &row[1] {
            TypedCell::Blob(bytes) => bytes.clone(),
            other => panic!("{raw_ssn} must be stored as BLOB ciphertext, got {other:?}"),
        };
        match &row[2] {
            TypedCell::Text(masked) => {
                assert_eq!(masked, "***-**-4321");
                assert_ne!(
                    masked, "987-65-4321",
                    "ssn (field's own column) must not hold plaintext"
                );
            }
            other => panic!("ssn (the masked column) must be TEXT, got {other:?}"),
        }

        let keys =
            zeroship_data_orm::encryption::KeyStore::new(crate::testing::isolate_key_source());
        let key = keys.resolve("default").await.expect("resolve key");
        let plaintext = zeroship_data_orm::encryption::aead::decrypt(
            &key,
            &stored_blob,
            &encryption::canonical_aad("default", "users", "ssn", row_id.as_bytes()),
        )
        .expect("decrypt updated ciphertext");
        assert_eq!(
            plaintext,
            b"987-65-4321".to_vec(),
            "non-id update must store ciphertext readable with the resolved row id"
        );
    });
}

#[test]
fn update_many_non_id_filter_encrypts_per_row_sqlite_runtime() {
    let _keys = with_project_key(&["default"], &"8".repeat(64));

    run(async {
        use rusqlite::types::Value as TypedCell;
        use zeroship_data_orm::encryption;

        let dir = tempfile::tempdir().expect("tempdir");
        let schema = users_encrypted_ssn_schema();
        apply_schema_ahead_of_runtime(&dir, &users_encrypted_ssn_ddl());
        let source = sqlite_runtime_source(
            "users",
            &schema,
            r#"
async function seed(_input, _ctx) {
    const coll = env.db.collection(COLLECTION);
    await coll.upsert(
        {
            email: "alice@example.com",
            name: "Red Team",
            ssn: "123-45-6789"
        },
        { conflictFields: ["email"] },
    );
    await coll.upsert(
        {
            email: "bob@example.com",
            name: "Red Team",
            ssn: "222-33-4444"
        },
        { conflictFields: ["email"] },
    );
    await coll.upsert(
        {
            email: "carol@example.com",
            name: "Blue Team",
            ssn: "555-66-7777"
        },
        { conflictFields: ["email"] },
    );
    return { seeded: 3 };
}
seed.config = { kind: "action" };

async function updateManyByName(_input, _ctx) {
    return await env.db.collection(COLLECTION).updateMany(
        { name: "Red Team" },
        { ssn: "999-88-7777" },
    );
}
updateManyByName.config = { kind: "action" };

const _procedures = { seed, updateManyByName };
"#,
        );

        dispatch_sqlite_runtime(&dir, &source, "seed");
        crate::live_tests::recording::clear();
        let updated =
            parity::extract_json(&dispatch_sqlite_runtime(&dir, &source, "updateManyByName"));
        assert_eq!(
            updated.as_f64(),
            Some(2.0),
            "two rows should match the non-id updateMany filter: {updated}"
        );
        let counters = crate::live_tests::recording::id_probes();
        assert_eq!(
            counters.len(),
            1,
            "encrypted updateMany must resolve one non-empty target set: {counters:?}"
        );
        assert!(
            !counters.is_empty(),
            "the target-resolution SQL set must be non-empty: {counters:?}"
        );
        let expected_limit = format!(" LIMIT {}", zeroship_data_sql::compile::MAX_QUERY_LIMIT + 1);
        for sql in &counters {
            assert!(
                sql.ends_with(&expected_limit),
                "updateMany target resolution must carry the row ceiling; sql={sql}"
            );
        }

        let client = crate::live_tests::sqlite::Inspector::open(dir.path());
        let raw_ssn = raw_column_name("ssn");
        let typed = client
            .query_typed(
                &format!(
                    r#"SELECT id, name, "{raw_ssn}", ssn
                   FROM "default"."users"
                   WHERE name = 'Red Team'
                   ORDER BY id"#
                ),
                &[],
            )
            .await
            .expect("SELECT typed updated rows");
        assert_eq!(typed.rows.len(), 2, "exactly two rows should be updated");

        let keys =
            zeroship_data_orm::encryption::KeyStore::new(crate::testing::isolate_key_source());
        let key = keys.resolve("default").await.expect("resolve key");
        for row in &typed.rows {
            let row_id = match &row[0] {
                TypedCell::Text(id) => id.clone(),
                other => panic!("id must be TEXT, got {other:?}"),
            };
            match &row[1] {
                TypedCell::Text(name) => assert_eq!(name, "Red Team"),
                other => panic!("name must be TEXT, got {other:?}"),
            }
            let stored_blob = match &row[2] {
                TypedCell::Blob(bytes) => bytes.clone(),
                other => panic!("{raw_ssn} must be stored as BLOB ciphertext, got {other:?}"),
            };
            match &row[3] {
                TypedCell::Text(masked) => {
                    assert_eq!(masked, "***-**-7777");
                    assert_ne!(
                        masked, "999-88-7777",
                        "ssn (field's own column) must not hold plaintext"
                    );
                }
                other => panic!("ssn (the masked column) must be TEXT, got {other:?}"),
            }
            let plaintext = zeroship_data_orm::encryption::aead::decrypt(
                &key,
                &stored_blob,
                &encryption::canonical_aad("default", "users", "ssn", row_id.as_bytes()),
            )
            .expect("decrypt updated ciphertext");
            assert_eq!(
                plaintext,
                b"999-88-7777".to_vec(),
                "each bulk-updated row must carry ciphertext bound to its own row id"
            );
        }
    });
}

#[test]
fn update_many_randomised_target_cap_rejects_without_writes_sqlite_runtime() {
    let _keys = with_project_key(&["default"], &"c".repeat(64));

    run(async {
        use rusqlite::types::Value as TypedCell;

        let dir = tempfile::tempdir().expect("tempdir");
        let target_cap = usize::try_from(zeroship_data_sql::compile::MAX_QUERY_LIMIT)
            .expect("MAX_QUERY_LIMIT must fit usize");
        let seeded = target_cap + 1;
        let values = (0..seeded)
            .map(|index| format!("('user_{index:04}', 'user_{index:04}@example.com', 'Red Team')"))
            .collect::<Vec<_>>();
        assert!(!values.is_empty(), "overflow fixture must seed target rows");
        let mut ddl = users_encrypted_ssn_ddl();
        ddl.push_str(&format!(
            "INSERT INTO \"default\".\"users\" (id, email, name) VALUES {};",
            values.join(",")
        ));
        apply_schema_ahead_of_runtime(&dir, &ddl);

        let schema = users_encrypted_ssn_schema();
        let source = sqlite_runtime_source(
            "users",
            &schema,
            r#"
async function overflow(_input, _ctx) {
    let failure = null;
    try {
        await env.db.collection(COLLECTION).updateMany(
            { name: "Red Team" },
            { ssn: "999-88-7777" },
        );
    } catch (err) {
        failure = {
            code: typeof err?.code === "string" ? err.code : null,
            message: err?.message ?? String(err),
        };
    }
    return { failure };
}
overflow.config = { kind: "action" };

const _procedures = { overflow };
"#,
        );

        crate::live_tests::recording::clear();
        let result = parity::extract_json(&dispatch_sqlite_runtime(&dir, &source, "overflow"));
        assert_eq!(
            result["failure"]["code"], "update_many_target_limit_exceeded",
            "the bounded probe must reject an overflowing target set: {result}"
        );
        let counters = crate::live_tests::recording::id_probes();
        assert_eq!(
            counters.len(),
            1,
            "overflow detection must use one bounded target probe: {counters:?}"
        );
        assert_eq!(
            counters.len(),
            1,
            "the overflow SQL witness set must contain exactly the exercised probe"
        );
        let expected_limit = format!(" LIMIT {}", target_cap + 1);
        assert!(
            counters[0].ends_with(&expected_limit),
            "the overflow probe must fetch at most one row beyond the write cap: {counters:?}"
        );

        let client = crate::live_tests::sqlite::Inspector::open(dir.path());
        let state = client
            .query_typed(
                r#"SELECT COUNT(*), SUM(version), COUNT(ssn)
                   FROM "default"."users"
                   WHERE name = 'Red Team'"#,
                &[],
            )
            .await
            .expect("inspect rows after overflowing updateMany");
        assert_eq!(
            state.rows.len(),
            1,
            "aggregate must return one non-empty row"
        );
        let expected_seeded = i64::try_from(seeded).expect("fixture count must fit i64");
        for (cell, expected, label) in [
            (&state.rows[0][0], expected_seeded, "row count"),
            (&state.rows[0][1], expected_seeded, "version sum"),
            (&state.rows[0][2], 0, "encrypted value count"),
        ] {
            match cell {
                TypedCell::Integer(actual) => assert_eq!(
                    *actual, expected,
                    "overflow rejection must preserve {label}"
                ),
                other => panic!("{label} must be INTEGER, got {other:?}"),
            }
        }
    });
}

#[test]
fn update_many_randomised_failure_rolls_back_committed_prefix_sqlite_runtime() {
    let _keys = with_project_key(&["default"], &"a".repeat(64));

    run(async {
        use rusqlite::types::Value as TypedCell;

        let dir = tempfile::tempdir().expect("tempdir");
        let schema = users_encrypted_ssn_schema();
        apply_schema_ahead_of_runtime(&dir, &users_encrypted_ssn_ddl());
        let source = sqlite_runtime_source(
            "users",
            &schema,
            r#"
async function seed(_input, _ctx) {
    const coll = env.db.collection(COLLECTION);
    // No `id` on either row: it is platform-assigned, so supplying one is
    // refused at the document boundary. Nothing below reads these ids - every
    // later assertion filters on `name` - so they were fixture convenience.
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
    return true;
}
seed.config = { kind: "action" };

async function failBulk(_input, _ctx) {
    const coll = env.db.collection(COLLECTION);
    let failure = null;
    try {
        await coll.updateMany(
            { name: "Red Team" },
            { email: "bulk-collision@example.com", ssn: "999-88-7777" },
        );
    } catch (err) {
        failure = {
            code: typeof err?.code === "string" ? err.code : null,
            message: err?.message ?? String(err),
        };
    }
    const after = await coll.find({ name: "Red Team" });
    return { failure, after };
}
failBulk.config = { kind: "action" };

async function failBulkInsideTransaction(_input, _ctx) {
    return await env.db.transaction(async (tx) => {
        let failure = null;
        try {
            await tx[COLLECTION].updateMany(
                { name: "Red Team" },
                { email: "nested-collision@example.com", ssn: "777-66-5555" },
            );
        } catch (err) {
            failure = {
                code: typeof err?.code === "string" ? err.code : null,
                message: err?.message ?? String(err),
            };
        }
        await tx[COLLECTION].insert({
            id: "control_row",
            email: "control@example.com",
            name: "Blue Team",
            ssn: "111-22-3333"
        });
        return { failure };
    });
}
failBulkInsideTransaction.config = { kind: "action" };

const _procedures = { seed, failBulk, failBulkInsideTransaction };
"#,
        );

        dispatch_sqlite_runtime(&dir, &source, "seed");
        crate::live_tests::recording::clear();
        let result = parity::extract_json(&dispatch_sqlite_runtime(&dir, &source, "failBulk"));
        let after = result["after"]
            .as_array()
            .expect("caught failure must leave an inspectable result set");
        assert_eq!(
            after.len(),
            2,
            "the exercised target set must be non-empty: {result}"
        );
        let counters = crate::live_tests::recording::id_probes();
        assert_eq!(
            counters.len(),
            1,
            "the failing call must exercise one per-row fan-out target query: {counters:?}"
        );
        assert!(
            !counters.is_empty(),
            "the failing fan-out SQL witness must be non-empty: {counters:?}"
        );
        assert_eq!(
            result["failure"]["code"], "unique_violation",
            "the second conflicting row must reject in creator vocabulary: {result}"
        );
        let mut caller_visible: Vec<(String, i64)> = after
            .iter()
            .map(|row| {
                (
                    row["email"]
                        .as_str()
                        .expect("caller-visible email must be a string")
                        .to_string(),
                    row["version"]
                        .as_i64()
                        .expect("caller-visible version must be an integer"),
                )
            })
            .collect();
        caller_visible.sort();
        assert_eq!(
            caller_visible,
            vec![
                ("alice@example.com".to_string(), 1),
                ("bob@example.com".to_string(), 1),
            ],
            "after rejection, the caller must observe that no prefix committed"
        );

        let client = crate::live_tests::sqlite::Inspector::open(dir.path());
        let typed = client
            .query_typed(
                r#"SELECT email, version
                   FROM "default"."users"
                   WHERE name = 'Red Team'
                   ORDER BY id"#,
                &[],
            )
            .await
            .expect("inspect rows after failed updateMany");
        assert_eq!(
            typed.rows.len(),
            2,
            "the directly inspected target set must be non-empty"
        );
        let expected = ["alice@example.com", "bob@example.com"];
        for (index, row) in typed.rows.iter().enumerate() {
            match &row[0] {
                TypedCell::Text(email) => assert_eq!(email, expected[index]),
                other => panic!("email must be TEXT, got {other:?}"),
            }
            match &row[1] {
                TypedCell::Integer(version) => assert_eq!(
                    *version, 1,
                    "a rejected updateMany must not commit or version-bump a prefix"
                ),
                other => panic!("version must be INTEGER, got {other:?}"),
            }
        }

        let nested = parity::extract_json(&dispatch_sqlite_runtime(
            &dir,
            &source,
            "failBulkInsideTransaction",
        ));
        // TWO things were stale here until 2026-09-01, and the first hid the
        // second.
        //
        // PATH: `transaction()` returns `Promise<Result<R>>` and wraps the
        // callback's value with `ok(...)` (`sdks/bootstrap/src/install-schema.ts`
        // :1145, :1193), so the payload is `{data: {...}, error: null}` and the
        // failure sits at `["data"]["failure"]`. Reading `["failure"]` yielded
        // `Null`, which compares unequal to ANY expected code - so this assertion
        // could never have passed, and could never have told you why.
        //
        // CASE: `canonicalErrorCode` (`sdks/db/src/errors.ts:27`) deliberately
        // upper-snakes every code not already in that form, so `unique_violation`
        // reaches app code as `UNIQUE_VIOLATION`. The lowercase driver spelling
        // survives only inside the nested `message` payload.
        assert_eq!(
            nested["data"]["failure"]["code"], "UNIQUE_VIOLATION",
            "the savepoint-wrapped fan-out must preserve the row error: {nested}"
        );
        let after_nested = client
            .query_typed(
                r#"SELECT email, version
                   FROM "default"."users"
                   WHERE name = 'Red Team'
                   ORDER BY id"#,
                &[],
            )
            .await
            .expect("inspect rows after nested failed updateMany");
        assert_eq!(
            after_nested.rows.len(),
            2,
            "nested target set must be non-empty"
        );
        for (index, row) in after_nested.rows.iter().enumerate() {
            match &row[0] {
                TypedCell::Text(email) => assert_eq!(email, expected[index]),
                other => panic!("email must be TEXT, got {other:?}"),
            }
            match &row[1] {
                TypedCell::Integer(version) => assert_eq!(*version, 1),
                other => panic!("version must be INTEGER, got {other:?}"),
            }
        }
        let control = client
            .query_typed(
                // KEYED ON EMAIL, NOT ON THE SUPPLIED id. The procedure inserts
                // `{ id: "control_row", ... }`, and the write path DISCARDS that
                // and mints a typed id - the row lands as
                // `user_034HHQXErG6U2Eb6CiCTu0`. `id` is in
                // IMMUTABLE_SYSTEM_FIELDS (`crud/system_fields_pass.rs:46`), and
                // on INSERT a caller-supplied value is replaced silently, where
                // an UPDATE touching the same field is refused loudly (`:399`,
                // `:428`). So `WHERE id = 'control_row'` matched nothing and this
                // assertion read 0 - which looked exactly like the outer
                // transaction having been rolled back, and was not.
                r#"SELECT COUNT(*) FROM "default"."users" WHERE email = 'control@example.com'"#,
                &[],
            )
            .await
            .expect("inspect outer transaction control write");
        assert!(
            !control.rows.is_empty(),
            "the outer transaction control result must be non-empty"
        );
        match &control.rows[0][0] {
            TypedCell::Integer(count) => assert_eq!(
                *count, 1,
                "rolling back the updateMany savepoint must not roll back the outer transaction"
            ),
            other => panic!("control count must be INTEGER, got {other:?}"),
        }
    });
}

#[test]
fn plain_updates_on_encrypted_collection_stay_on_fast_path_sqlite_runtime() {
    let _keys = with_project_key(&["default"], &"9".repeat(64));

    run(async {
        let dir = tempfile::tempdir().expect("tempdir");
        let schema = users_encrypted_ssn_schema();
        apply_schema_ahead_of_runtime(&dir, &users_encrypted_ssn_ddl());
        let source = sqlite_runtime_source(
            "users",
            &schema,
            r#"
async function seed(_input, _ctx) {
    const coll = env.db.collection(COLLECTION);
    await coll.upsert(
        {
            email: "alice@example.com",
            name: "Red Team",
            ssn: "123-45-6789"
        },
        { conflictFields: ["email"] },
    );
    await coll.upsert(
        {
            email: "bob@example.com",
            name: "Red Team",
            ssn: "222-33-4444"
        },
        { conflictFields: ["email"] },
    );
    return true;
}
seed.config = { kind: "action" };

async function updatePlain(_input, _ctx) {
    return await env.db.collection(COLLECTION).update(
        { email: "alice@example.com" },
        { name: "Blue Team" },
    );
}
updatePlain.config = { kind: "action" };

async function updateManyPlain(_input, _ctx) {
    return await env.db.collection(COLLECTION).updateMany(
        { name: "Red Team" },
        { name: "Green Team" },
    );
}
updateManyPlain.config = { kind: "action" };

const _procedures = { seed, updatePlain, updateManyPlain };
"#,
        );

        dispatch_sqlite_runtime(&dir, &source, "seed");

        crate::live_tests::recording::clear();
        let updated = parity::extract_json(&dispatch_sqlite_runtime(&dir, &source, "updatePlain"));
        assert_eq!(
            updated.get("name").and_then(|v| v.as_str()),
            Some("Blue Team"),
            "plain update should still update the targeted row",
        );
        assert_write_path_fast_path("updateOne plain field");

        crate::live_tests::recording::clear();
        let updated_many =
            parity::extract_json(&dispatch_sqlite_runtime(&dir, &source, "updateManyPlain"));
        assert_eq!(
            updated_many.as_f64(),
            Some(1.0),
            "plain updateMany should only affect the remaining Red Team row",
        );
        assert_write_path_fast_path("updateMany plain field");
    });
}

#[test]
fn plain_upsert_on_encrypted_collection_skips_conflict_probe_sqlite_runtime() {
    let _keys = with_project_key(&["default"], &"a".repeat(64));

    run(async {
        let dir = tempfile::tempdir().expect("tempdir");
        let schema = users_encrypted_secret_schema();
        apply_schema_ahead_of_runtime(&dir, &users_encrypted_secret_ddl());
        let source = sqlite_runtime_source(
            "users",
            &schema,
            r#"
async function seed(_input, _ctx) {
    return await env.db.collection(COLLECTION).upsert(
        {
            email: "alice@example.com",
            name: "Alice",
            secret: "alpha-secret"
        },
        { conflictFields: ["email"] },
    );
}
seed.config = { kind: "action" };

async function upsertPlainConflict(_input, _ctx) {
    return await env.db.collection(COLLECTION).upsert(
        {
            email: "alice@example.com",
            name: "Alice Updated"
        },
        { conflictFields: ["email"] },
    );
}
upsertPlainConflict.config = { kind: "action" };

const _procedures = { seed, upsertPlainConflict };
"#,
        );

        let seeded = dispatch_sqlite_runtime(&dir, &source, "seed");
        let seed_row = parity::extract_json(&seeded);
        let seed_id = seed_row["id"].as_str().expect("generated id");

        crate::live_tests::recording::clear();
        let updated = parity::extract_json(&dispatch_sqlite_runtime(
            &dir,
            &source,
            "upsertPlainConflict",
        ));
        assert_eq!(
            updated.get("id").and_then(|v| v.as_str()),
            Some(seed_id),
            "plain conflict upsert should still target the existing row",
        );
        assert_eq!(
            updated.get("name").and_then(|v| v.as_str()),
            Some("Alice Updated"),
            "plain conflict upsert should still update the plain field",
        );
        assert_write_path_fast_path("upsert plain field");
    });
}

#[test]
fn update_rejects_nested_version_filter_without_mutating_sqlite_row() {
    let _keys = with_project_key(&["default"], &"1".repeat(64));

    run(async {
        let dir = tempfile::tempdir().expect("tempdir");
        let schema = users_encrypted_ssn_schema();
        apply_schema_ahead_of_runtime(&dir, &users_encrypted_ssn_ddl());
        let source = sqlite_runtime_source(
            "users",
            &schema,
            r#"
async function seed(_input, _ctx) {
    return await env.db.collection(COLLECTION).upsert(
        {
            email: "alice@example.com",
            name: "Alice",
            ssn: "123-45-6789"
        },
        { conflictFields: ["email"] },
    );
}
seed.config = { kind: "action" };

async function nestedCasUpdate(_input, _ctx) {
    return await env.db.collection(COLLECTION).update(
        {
            "$and": [
                { email: "alice@example.com" },
                { version: 1 }
            ]
        },
        { name: "Mallory" },
    );
}
nestedCasUpdate.config = { kind: "action" };

const _procedures = { seed, nestedCasUpdate };
"#,
        );

        let seeded = dispatch_sqlite_runtime(&dir, &source, "seed");
        let row = parity::extract_json(&seeded);
        assert_eq!(
            row.get("version").and_then(|v| v.as_i64()),
            Some(1),
            "seed row must start at version 1"
        );

        let (status, body) = parity::dispatch_zs_with_descriptor(
            &parity::sqlite_url(&dir),
            &source.source,
            "nestedCasUpdate",
            parity::DEV_APP_ID,
            &source.descriptor,
        );
        assert_ne!(status, 200, "nested version CAS must reject, got {body}");
        assert_eq!(
            body.get("code").and_then(|v| v.as_str()),
            Some("version_filter_must_be_top_level"),
            "nested CAS rejection must carry the canonical code: {body}"
        );

        let client = crate::live_tests::sqlite::Inspector::open(dir.path());
        let rows = client
            .query(
                r#"SELECT name, version FROM "default"."users" WHERE email = 'alice@example.com'"#,
                &[],
            )
            .await
            .expect("SELECT row after rejected nested CAS update");
        assert_eq!(rows.len(), 1, "seed row must still exist");
        assert_eq!(
            rows[0][0].as_deref(),
            Some("Alice"),
            "failed nested CAS update must not rewrite the row"
        );
        assert_eq!(
            rows[0][1].as_deref(),
            Some("1"),
            "failed nested CAS update must not auto-bump version"
        );
    });
}

#[test]
fn update_many_rejects_nested_version_filter_without_mutating_sqlite_row() {
    let _keys = with_project_key(&["default"], &"2".repeat(64));

    run(async {
        let dir = tempfile::tempdir().expect("tempdir");
        let schema = users_encrypted_ssn_schema();
        apply_schema_ahead_of_runtime(&dir, &users_encrypted_ssn_ddl());
        let source = sqlite_runtime_source(
            "users",
            &schema,
            r#"
async function seed(_input, _ctx) {
    return await env.db.collection(COLLECTION).upsert(
        {
            email: "alice@example.com",
            name: "Alice",
            ssn: "123-45-6789"
        },
        { conflictFields: ["email"] },
    );
}
seed.config = { kind: "action" };

async function nestedCasUpdateMany(_input, _ctx) {
    return await env.db.collection(COLLECTION).updateMany(
        {
            "$and": [
                { email: "alice@example.com" },
                { version: 1 }
            ]
        },
        { name: "Mallory" },
    );
}
nestedCasUpdateMany.config = { kind: "action" };

const _procedures = { seed, nestedCasUpdateMany };
"#,
        );

        dispatch_sqlite_runtime(&dir, &source, "seed");

        let (status, body) = parity::dispatch_zs_with_descriptor(
            &parity::sqlite_url(&dir),
            &source.source,
            "nestedCasUpdateMany",
            parity::DEV_APP_ID,
            &source.descriptor,
        );
        assert_ne!(status, 200, "nested version CAS must reject, got {body}");
        assert_eq!(
            body.get("code").and_then(|v| v.as_str()),
            Some("version_filter_must_be_top_level"),
            "nested CAS rejection must carry the canonical code: {body}"
        );

        let client = crate::live_tests::sqlite::Inspector::open(dir.path());
        let rows = client
            .query(
                r#"SELECT name, version FROM "default"."users" WHERE email = 'alice@example.com'"#,
                &[],
            )
            .await
            .expect("SELECT row after rejected nested CAS updateMany");
        assert_eq!(rows.len(), 1, "seed row must still exist");
        assert_eq!(
            rows[0][0].as_deref(),
            Some("Alice"),
            "failed nested CAS updateMany must not rewrite the row"
        );
        assert_eq!(
            rows[0][1].as_deref(),
            Some("1"),
            "failed nested CAS updateMany must not auto-bump version"
        );
    });
}

#[allow(unused_imports)]
use zeroship_data_orm::protection::Catalog;

#[cfg(test)]
#[allow(unused_imports)]
use zeroship_data_orm::search::Search;

#[test]
fn encrypted_conflict_target_is_refused_sqlite_runtime() {
    let dir = tempfile::tempdir().expect("tempdir");
    let schema = users_encrypted_ssn_schema();
    apply_schema_ahead_of_runtime(&dir, &users_encrypted_ssn_ddl());
    let source = sqlite_runtime_source(
        "users",
        &schema,
        r#"
async function rejectConflict() {
  try {
    await env.db.collection(COLLECTION).upsert(
      { email: "a@example.com", name: "Alice", ssn: "secret" },
      { conflictFields: ["ssn"] },
    );
    return { accepted: true };
  } catch (error) { return { code: error.code }; }
}
rejectConflict.config = { kind: "action" };
const _procedures = { rejectConflict };
"#,
    );
    let result = parity::extract_json(&dispatch_sqlite_runtime(&dir, &source, "rejectConflict"));
    assert_eq!(result["code"], "encrypted_conflict_field", "{result}");
}

/// The seven system columns and the `["id"]` primary key, SQLite spelling.
const SYSTEM_COLUMNS_SQLITE: &str = r#"
  id TEXT PRIMARY KEY,
  created_at TEXT NOT NULL DEFAULT CURRENT_TIMESTAMP,
  updated_at TEXT NOT NULL DEFAULT CURRENT_TIMESTAMP,
  created_by TEXT NULL,
  updated_by TEXT NULL,
  version INTEGER NOT NULL DEFAULT 1,
  deleted_at TEXT NULL"#;
