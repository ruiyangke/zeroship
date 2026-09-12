//! SQLite updates contracts.
use super::fixtures::*;

use crate::tests::fixtures::parity;

use zeroship_data_orm::sql::mapping::raw_column_name;

#[test]
fn bulk_mutations_return_counts_without_returning_records_sqlite_runtime() {
    run(async {
        let dir = tempfile::tempdir().unwrap();
        apply_schema_ahead_of_runtime(&dir, &users_encrypted_ssn_ddl());
        let total = zeroship_data_orm::sql::MAX_ROW_LIMIT + 17;
        let body = r#"
async function bulk() {
    const users = env.db.collection(COLLECTION);
    const total = BULK_TOTAL;
    for (let start = 0; start < total; start += 100) {
        await users.insertMany(Array.from({ length: Math.min(100, total - start) }, (_, i) => ({
            email: `user-${start + i}@example.com`, name: "new"
        })));
    }
    const updated = await users.updateMany({}, { name: "ready" });
    const missing = await users.updateMany({ name: "absent" }, { name: "unused" });
    try {
        await env.db.transaction(async tx => {
            await tx[COLLECTION].purgeMany({});
            throw new Error("rollback");
        });
    } catch (error) {
        if (error.message !== "rollback") throw error;
    }
    const deleted = await users.deleteMany({});
    const restored = await users.restoreMany({});
    const purged = await users.purgeMany({});
    return { updated, missing, deleted, restored, purged, remaining: await users.count({}) };
}
bulk.config = { kind: "action" };
const _procedures = { bulk };
"#
        .replace("BULK_TOTAL", &total.to_string());
        let mut source = sqlite_runtime_source("users", &users_encrypted_ssn_schema(), &body);
        source.descriptor = serde_json::to_string(&zeroship_data_orm::value!({
            "version":2,
            "collections":{"users":{
                "fields":crate::tests::fixtures::schema::generated_fields(users_encrypted_ssn_schema()),
                "options":{"softDelete":true, "versioning":true, "strictness":"strict"},
                "indexes":[],
            }},
        })).unwrap();
        crate::tests::fixtures::recording::clear();
        let result = parity::extract_json(&dispatch_sqlite_runtime(&dir, &source, "bulk"));
        assert_eq!(
            result,
            zeroship_data_orm::value!({
                "updated":total, "missing":0, "deleted":total, "restored":total,
                "purged":total, "remaining":0,
            })
        );
        let statements = crate::tests::fixtures::recording::bulk_statements();
        assert_eq!(statements.len(), 6);
        for sql in statements {
            assert!(!sql.contains(" RETURNING "), "{sql}");
        }
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

        let client = crate::tests::fixtures::sqlite::Inspector::open(dir.path());
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
            zeroship_data_orm::encryption::KeyStore::new(crate::tests::fixtures::key_source());
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
        crate::tests::fixtures::recording::clear();
        let updated =
            parity::extract_json(&dispatch_sqlite_runtime(&dir, &source, "updateManyByName"));
        assert_eq!(
            updated.as_f64(),
            Some(2.0),
            "two rows should match the non-id updateMany filter: {updated}"
        );
        let counters = crate::tests::fixtures::recording::id_probes();
        assert_eq!(
            counters.len(),
            1,
            "encrypted updateMany must resolve one non-empty target set: {counters:?}"
        );
        assert!(
            !counters.is_empty(),
            "the target-resolution SQL set must be non-empty: {counters:?}"
        );
        let expected_limit = zeroship_data_orm::value::Value::from(
            zeroship_data_orm::budgets::MAX_PER_ROW_UPDATE_TARGETS + 1,
        );
        for query in &counters {
            assert!(
                query.sql.ends_with(" LIMIT $2"),
                "updateMany target resolution must bind the row ceiling; query={query:?}"
            );
            assert_eq!(query.params.last(), Some(&expected_limit));
        }

        let client = crate::tests::fixtures::sqlite::Inspector::open(dir.path());
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
            zeroship_data_orm::encryption::KeyStore::new(crate::tests::fixtures::key_source());
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
        let target_cap = zeroship_data_orm::budgets::MAX_PER_ROW_UPDATE_TARGETS;
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

        crate::tests::fixtures::recording::clear();
        let result = parity::extract_json(&dispatch_sqlite_runtime(&dir, &source, "overflow"));
        assert_eq!(
            result["failure"]["code"], "update_many_target_limit_exceeded",
            "the bounded probe must reject an overflowing target set: {result}"
        );
        let counters = crate::tests::fixtures::recording::id_probes();
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
        let expected_limit = zeroship_data_orm::value::Value::from(target_cap + 1);
        assert!(
            counters[0].sql.ends_with(" LIMIT $2"),
            "the overflow probe must fetch at most one row beyond the write cap: {counters:?}"
        );
        assert_eq!(counters[0].params.last(), Some(&expected_limit));

        let client = crate::tests::fixtures::sqlite::Inspector::open(dir.path());
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
        crate::tests::fixtures::recording::clear();
        let result = parity::extract_json(&dispatch_sqlite_runtime(&dir, &source, "failBulk"));
        let after = result["after"]
            .as_array()
            .expect("caught failure must leave an inspectable result set");
        assert_eq!(
            after.len(),
            2,
            "the exercised target set must be non-empty: {result}"
        );
        let counters = crate::tests::fixtures::recording::id_probes();
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

        let client = crate::tests::fixtures::sqlite::Inspector::open(dir.path());
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
                // Find the control insert by its unique fixture email.
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

        crate::tests::fixtures::recording::clear();
        let updated = parity::extract_json(&dispatch_sqlite_runtime(&dir, &source, "updatePlain"));
        assert_eq!(
            updated.get("name").and_then(|v| v.as_str()),
            Some("Blue Team"),
            "plain update should still update the targeted row",
        );
        assert_write_path_fast_path("updateOne plain field");

        crate::tests::fixtures::recording::clear();
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
fn update_rejects_nested_version_filter_without_mutating_sqlite_row() {
    let _keys = with_project_key(&["default"], &"1".repeat(64));

    run(async {
        let dir = tempfile::tempdir().expect("tempdir");
        let schema = users_encrypted_ssn_schema();
        apply_schema_ahead_of_runtime(&dir, &users_encrypted_ssn_ddl());
        let mut source = sqlite_runtime_source(
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
        let mut descriptor: serde_json::Value = serde_json::from_str(&source.descriptor).unwrap();
        descriptor["collections"]["users"]["options"]["versioning"] = true.into();
        descriptor["collections"]["users"]["fields"]["version"]["concurrency"] = true.into();
        source.descriptor = descriptor.to_string();

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

        let client = crate::tests::fixtures::sqlite::Inspector::open(dir.path());
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
        let mut source = sqlite_runtime_source(
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
        let mut descriptor: serde_json::Value = serde_json::from_str(&source.descriptor).unwrap();
        descriptor["collections"]["users"]["options"]["versioning"] = true.into();
        descriptor["collections"]["users"]["fields"]["version"]["concurrency"] = true.into();
        source.descriptor = descriptor.to_string();

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

        let client = crate::tests::fixtures::sqlite::Inspector::open(dir.path());
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
