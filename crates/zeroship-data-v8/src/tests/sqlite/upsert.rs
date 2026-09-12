//! SQLite upsert contracts.
use super::fixtures::*;

use crate::tests::fixtures::parity;

use zeroship_data_orm::sql::mapping::raw_column_name;

/// One randomised-encrypted column and no mask - the fast-path fixtures assert
/// a PLAIN write skips row resolution, so the encrypted column must exist but
/// stay untouched by the write under test.
fn users_encrypted_secret_schema() -> zeroship_data_orm::value::Value {
    zeroship_data_orm::value!({
        "email": {"type": "string", "required": true, "unique": true},
        "name": {"type": "string", "required": true},
        "secret": {
            "type": "string",
            "encrypted": true
        }
    })
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

        let client = crate::tests::fixtures::sqlite::Inspector::open(dir.path());
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
                .is_none_or(zeroship_data_orm::value::Value::is_null),
            "a supplied created_by must not land on the insert arm: {first:?}"
        );
        assert!(
            second
                .get("created_by")
                .is_none_or(zeroship_data_orm::value::Value::is_null),
            "a supplied created_by must not land on the conflict arm: {second:?}"
        );
        assert!(
            second
                .get("updated_by")
                .is_none_or(zeroship_data_orm::value::Value::is_null),
            "a supplied updated_by must not land on the conflict arm: {second:?}"
        );
        assert_eq!(
            second.get("version").and_then(|v| v.as_i64()),
            Some(2),
            "conflict update must auto-bump version"
        );

        let client = crate::tests::fixtures::sqlite::Inspector::open(dir.path());
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
            zeroship_data_orm::encryption::KeyStore::new(crate::tests::fixtures::key_source());
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

        crate::tests::fixtures::recording::clear();
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
