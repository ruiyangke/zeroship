//! SQLite encryption contracts.
use super::fixtures::*;

use crate::tests::fixtures::schema::fixture_table_sql_for;
use crate::tests::fixtures::Host;

use zeroship_migrate::schema::query::FkEmission;

use zeroship_data_orm::error::DbError;

use crate::sql::compile::raw_column_name;

#[cfg(test)]
use crate::tests::fixtures::DatabaseFixture;

/// Bind a raw byte slice as a SQLite BLOB literal using the `X'...'`
/// hex syntax. SQLite accepts this anywhere a value literal can appear,
/// and the session-actor's `&str`-params channel can carry inline
/// literals untouched. Returns the literal text including the `X'`
/// prefix and closing `'`.
fn sqlite_blob_literal(bytes: &[u8]) -> String {
    let mut s = String::with_capacity(3 + bytes.len() * 2);
    s.push_str("X'");
    for b in bytes {
        use std::fmt::Write as _;
        let _ = write!(s, "{b:02x}");
    }
    s.push('\'');
    s
}

#[test]
fn insert_many_encrypts_ciphertext_before_sqlite_storage() {
    Host::test(|host| {
        host.run(async {
            use std::collections::HashMap;

            use crate::sql::compile::SqlDialect;
            use zeroship_data_orm::backend::sqlite::session::TypedCell;
            use zeroship_data_orm::encryption;

            let _keys = host.supply_project_key(&["app_demo"], &"d".repeat(64));
            let app_id = "app_demo";
            let collection = "bulk_people";
            let schema = crate::tests::fixtures::schema::generated_fields(crate::value!({
                "name": { "type": "string" },
                "ssn": {
                    "type": "string",
                    "encrypted": true,
                    "mask": { "kind": "last4", "classification": "spi" }
                }
            }));
            let (backend, _dir) =
                unmask_setup_with_schema(host, app_id, collection, schema.clone()).await;
            let ddl = fixture_table_sql_for(
                &crate::sql::SchemaName::new(app_id).expect("fixture schema name"),
                collection,
                &schema,
                &FkEmission::Inline,
                SqlDialect::Sqlite,
            )
            .expect("build DDL");
            for stmt in ddl.split(";\n") {
                let trimmed = stmt.trim();
                if trimmed.is_empty() {
                    continue;
                }
                backend
                    .execute_fixture(trimmed, &[])
                    .await
                    .expect("DDL exec");
            }

            let mut docs = crate::value!([
                { "name": "Alice", "ssn": "123-45-6789" },
                { "name": "Bob", "ssn": "987-65-4321" }
            ]);
            host.prepare_insert_many_docs(&mut docs, app_id, collection, Some("usr_bulk_writer"))
                .await
                .expect("prepare insertMany docs");

            let expected_by_id: HashMap<String, (Vec<u8>, String)> = docs
                .as_array()
                .expect("docs array")
                .iter()
                .map(|doc| {
                    let obj = doc.as_object().expect("doc object");
                    (
                        obj.get("id")
                            .and_then(|v| v.as_str())
                            .expect("minted id")
                            .to_string(),
                        (
                            obj.get(raw_column_name("ssn").as_str())
                                .and_then(|v| v.as_bytes())
                                .expect("native ciphertext in the raw column")
                                .to_vec(),
                            obj.get("ssn")
                                .and_then(|v| v.as_str())
                                .expect("masked sibling stays on the field's own column")
                                .to_string(),
                        ),
                    )
                })
                .collect();

            let built = compile_insert_many(
                &crate::sql::SchemaName::new(app_id).expect("fixture schema name"),
                collection,
                &schema,
                &docs,
                SqlDialect::Sqlite,
            )
            .expect("build insertMany");
            let params = &built.params;
            let client = backend
                .fixture_session(app_id)
                .await
                .expect("acquire client");
            client
                .query_typed(&built.sql, params)
                .await
                .expect("INSERT ... RETURNING");

            let raw_ssn = raw_column_name("ssn");
            let typed = client
                .query_typed(
                    &format!(
                        r#"SELECT id, "{raw_ssn}", ssn FROM "{app_id}"."{collection}" ORDER BY id"#
                    ),
                    &[],
                )
                .await
                .expect("SELECT typed");
            assert_eq!(typed.rows.len(), 2, "two rows stored");

            let key = backend
                .key_store()
                .resolve(app_id)
                .await
                .expect("resolve key");
            for row in &typed.rows {
                let id = match &row[0] {
                    TypedCell::Text(s) => s.clone(),
                    other => panic!("id must be TEXT, got {other:?}"),
                };
                let stored_blob = match &row[1] {
                    TypedCell::Blob(bytes) => bytes.clone(),
                    other => panic!("{raw_ssn} must be stored as BLOB ciphertext, got {other:?}"),
                };
                let masked = match &row[2] {
                    TypedCell::Text(s) => s.clone(),
                    other => panic!("ssn (the masked column) must be TEXT, got {other:?}"),
                };
                let (prepared_ciphertext, prepared_masked) = expected_by_id
                    .get(&id)
                    .expect("stored row id should match prepared docs");
                assert_eq!(masked, *prepared_masked, "masked sibling must be persisted");
                assert_ne!(
                    stored_blob,
                    b"123-45-6789".to_vec(),
                    "stored bytes must not equal raw plaintext",
                );
                assert_ne!(
                    stored_blob,
                    b"987-65-4321".to_vec(),
                    "stored bytes must not equal raw plaintext",
                );
                // The field's own column (`ssn`) must never hold the real value:
                // it is the masked sibling's new home after the storage flip.
                assert_ne!(
                    masked, "123-45-6789",
                    "ssn (field's own column) must not hold plaintext",
                );
                assert_ne!(
                    masked, "987-65-4321",
                    "ssn (field's own column) must not hold plaintext",
                );
                let expected_ciphertext = prepared_ciphertext.clone();
                assert_eq!(
                    stored_blob, expected_ciphertext,
                    "raw stored bytes must match the write-side ciphertext",
                );
                let plaintext = zeroship_data_orm::encryption::aead::decrypt(
                    &key,
                    &stored_blob,
                    &encryption::canonical_aad(app_id, collection, "ssn", id.as_bytes()),
                )
                .expect("decrypt stored blob");
                assert!(
                    plaintext == b"123-45-6789" || plaintext == b"987-65-4321",
                    "decrypting the stored blob must recover one of the inserted plaintexts",
                );
            }
        });
    })
}

/// Round-trip an encrypted string through file-backed SQLite with a supplied
/// test key. Reading the stored ciphertext recovers the original plaintext.
#[test]
fn encrypted_column_round_trip_sqlite_randomised() {
    Host::test(|host| {
        use zeroship_data_orm::encryption;

        let _keys = host.supply_project_key(&["app1"], &"a".repeat(64));
        host.run(async {
            let (backend, _dir) = fresh_backend(host);
            backend
                .attach_app_file("app_demo")
                .await
                .expect("ensure_app_schema");
            backend
                .execute_fixture(
                    "CREATE TABLE \"app_demo\".\"enc_notes\" (\
                     id  TEXT PRIMARY KEY, \
                     ssn BLOB\
                 )",
                    &[],
                )
                .await
                .expect("CREATE TABLE enc_notes");

            let key = backend
                .key_store()
                .resolve("app1")
                .await
                .expect("resolve_key");
            let plaintext = b"123-45-6789";
            let aad = encryption::canonical_aad("app1", "enc_notes", "ssn", b"row_a");
            let ct = zeroship_data_orm::encryption::aead::encrypt(&key, plaintext, &aad)
                .expect("encrypt");

            // Bind the ciphertext as an inline X'...' BLOB literal. The
            // session actor's `[&str]` params lane only carries TEXT; SQL
            // literals are how we inject BLOB values without widening the
            // protocol.
            let blob_lit = sqlite_blob_literal(&ct);
            let insert_sql =
                format!("INSERT INTO \"app_demo\".\"enc_notes\" (id, ssn) VALUES (?, {blob_lit})");
            backend
                .execute_fixture(&insert_sql, &[("row_a").into()])
                .await
                .expect("INSERT");

            // Pull the ciphertext back as a typed BLOB. `execute_fixture_on`
            // routes through `query`, which stringifies BLOBs as
            // `<N bytes blob>` — that's not what we want here. Reach into
            // the session's typed-row path via the dedicated client; the
            // `query_typed` method preserves the BLOB discriminant. The
            // simplest cross-test path: re-encode the BLOB as hex via SQL
            // (`hex(ssn)`) and parse back to bytes here.
            let client = backend
                .fixture_session("app_demo")
                .await
                .expect("acquire client");
            let rows = client
                .query(
                    "SELECT hex(ssn) FROM \"app_demo\".\"enc_notes\" WHERE id = ?",
                    &["row_a"],
                )
                .await
                .expect("SELECT");
            let hex_str = rows[0][0].clone().expect("ssn column must be present");
            let raw: Vec<u8> = (0..hex_str.len())
                .step_by(2)
                .map(|i| u8::from_str_radix(&hex_str[i..i + 2], 16).unwrap())
                .collect();

            let recovered =
                zeroship_data_orm::encryption::aead::decrypt(&key, &raw, &aad).expect("decrypt");
            assert_eq!(recovered, plaintext);
        });
    })
}

/// **§13 Camp A fence (SQLite half), mirror of the PG test
/// `encrypted_randomised_row_swap_rejected`**. Insert encrypted
/// rows; UPDATE swaps their ciphertexts; reading row B with row B's
/// AAD must surface `encryption_aead_failed`. This is the load-bearing
/// assertion for the row-PK-in-AAD policy.
#[test]
fn randomised_ciphertext_row_swap_rejected_sqlite() {
    Host::test(|host| {
        use zeroship_data_orm::encryption;

        let _keys = host.supply_project_key(&["app1"], &"d".repeat(64));
        host.run(async {
            let (backend, _dir) = fresh_backend(host);
            backend
                .attach_app_file("app_demo")
                .await
                .expect("ensure_app_schema");
            backend
                .execute_fixture(
                    "CREATE TABLE \"app_demo\".\"enc_notes\" (\
                     id  TEXT PRIMARY KEY, \
                     ssn BLOB\
                 )",
                    &[],
                )
                .await
                .expect("CREATE TABLE enc_notes");

            let key = backend.key_store().resolve("app1").await.unwrap();
            // Insert row A and row B, each with its OWN AAD (binds row_pk).
            let ct_a = zeroship_data_orm::encryption::aead::encrypt(
                &key,
                b"sensitive-A",
                &encryption::canonical_aad("app1", "enc_notes", "ssn", b"row_a"),
            )
            .unwrap();
            let ct_b = zeroship_data_orm::encryption::aead::encrypt(
                &key,
                b"sensitive-B",
                &encryption::canonical_aad("app1", "enc_notes", "ssn", b"row_b"),
            )
            .unwrap();
            for (id, ct) in [("row_a", &ct_a), ("row_b", &ct_b)] {
                let blob_lit = sqlite_blob_literal(ct);
                let sql = format!(
                    "INSERT INTO \"app_demo\".\"enc_notes\" (id, ssn) VALUES (?, {blob_lit})"
                );
                backend.execute_fixture(&sql, &[(id).into()]).await.unwrap();
            }

            // Attacker move: UPDATE row_b's ssn slot with row_a's ciphertext.
            let blob_a = sqlite_blob_literal(&ct_a);
            let sql = format!("UPDATE \"app_demo\".\"enc_notes\" SET ssn = {blob_a} WHERE id = ?");
            backend
                .execute_fixture(&sql, &[("row_b").into()])
                .await
                .unwrap();

            // Read row B's ssn back and try to decrypt with row B's AAD.
            let client = backend
                .fixture_session("app_demo")
                .await
                .expect("acquire client");
            let rows = client
                .query(
                    "SELECT hex(ssn) FROM \"app_demo\".\"enc_notes\" WHERE id = ?",
                    &["row_b"],
                )
                .await
                .unwrap();
            let hex_str = rows[0][0].clone().expect("ssn must be present");
            let raw: Vec<u8> = (0..hex_str.len())
                .step_by(2)
                .map(|i| u8::from_str_radix(&hex_str[i..i + 2], 16).unwrap())
                .collect();
            let aad_b = encryption::canonical_aad("app1", "enc_notes", "ssn", b"row_b");
            let err = zeroship_data_orm::encryption::aead::decrypt(&key, &raw, &aad_b)
                .expect_err("row-swap must fail AAD verification");
            match err {
                DbError::ValidationFailed { code, .. } => {
                    assert_eq!(code, "encryption_aead_failed");
                }
                other => panic!("expected ValidationFailed/encryption_aead_failed, got {other:?}"),
            }
        });
    })
}

/// Backend instances with the same supplied project key can decrypt each other’s
/// ciphertext when the app, field and row identity also match.
#[test]
fn cross_backend_ciphertext_decrypt_via_shared_key() {
    Host::test(|host| {
        use zeroship_data_orm::encryption;

        let _keys = host.supply_project_key(&["app_shared"], &"e".repeat(64));
        host.run(async {
            // Two separate backends rooted at separate temp dirs.
            let (backend_a, _dir_a) = fresh_backend(host);
            let (backend_b, _dir_b) = fresh_backend(host);

            // Both backends receive the same explicit app-to-project binding.
            let app_id = "app_shared";
            let key_a = backend_a.key_store().resolve(app_id).await.unwrap();
            let key_b = backend_b.key_store().resolve(app_id).await.unwrap();
            // Both handles must resolve the supplied project key.
            assert_eq!(key_a.k_enc, key_b.k_enc);

            let plaintext = b"cross-instance-payload";
            let aad = encryption::canonical_aad(app_id, "enc_notes", "ssn", b"row_a");
            let ct = zeroship_data_orm::encryption::aead::encrypt(&key_a, plaintext, &aad)
                .expect("encrypt on A");

            // Decrypt the SAME ciphertext on backend_b with backend_b's
            // resolved key. Must round-trip.
            let recovered = zeroship_data_orm::encryption::aead::decrypt(&key_b, &ct, &aad)
                .expect("decrypt on B");
            assert_eq!(recovered, plaintext);
        });
    })
}

/// Round-trip encryption through SQLite SQL binding and typed decoding.
/// This exercises the Rust pipeline directly.
#[test]
fn encrypted_column_e2e_crud_round_trip_sqlite() {
    Host::test(|host| {
        use crate::sql::compile::SqlDialect;
        use crate::tests::fixtures::DatabaseFixture;
        use zeroship_data_orm::backend::sqlite::session::TypedCell;
        use zeroship_data_orm::protection::encryption_pass::{
            decrypt_row_on_read, encrypt_row_on_write,
        };

        let _keys = host.supply_project_key(&["app_demo"], &"c".repeat(64));
        host.run(async {
            let (backend, _dir) = fresh_backend(host);
            backend
                .attach_app_file("app_demo")
                .await
                .expect("ensure_app_schema");
            // Only ciphertext binding is under test, so this fixture omits catalog sentinels.
            backend
                .execute_fixture(
                    "CREATE TABLE \"app_demo\".\"users\" (\
                     id  TEXT PRIMARY KEY, \
                     ssn BLOB\
                 )",
                    &[],
                )
                .await
                .expect("CREATE TABLE users");

            // The field descriptor selects encryption; the host supplies the project key.
            let schema = crate::value!({
                "id": {"type":"string", "primaryKey":true},
                "ssn": {
                    "type": "string",
                    "encrypted": true,
                },
            });

            let plaintext = "123-45-6789";
            let row_pk = "row_e2e";
            let mut doc = crate::value!({
                "id": row_pk,
                "ssn": plaintext,
            });

            // The encryption pass replaces ssn with native ciphertext bytes.
            encrypt_row_on_write(
                backend.key_store(),
                "app_demo",
                "users",
                &schema,
                row_pk,
                &mut doc,
            )
            .await
            .expect("encrypt_row_on_write");
            let ciphertext = doc["ssn"].as_bytes().expect("native ciphertext").to_vec();

            // Bind the ciphertext directly.
            let bq = compile_insert(
                &crate::sql::SchemaName::new("app_demo").expect("fixture schema name"),
                "users",
                &schema,
                &doc,
                SqlDialect::Sqlite,
            )
            .expect("build_insert_with_dialect");
            assert!(
                !bq.sql.contains("decode("),
                "SQLite dialect must not emit `decode(...)::bytea`: {}",
                bq.sql,
            );
            assert!(bq
                .params
                .iter()
                .any(|value| value.as_bytes() == Some(ciphertext.as_slice())));
            assert!(!bq.sql.contains("unhex("));

            // Execute the compiled INSERT through the typed RETURNING surface.
            let param_refs = &bq.params;
            let client = backend
                .fixture_session("app_demo")
                .await
                .expect("acquire client");
            let _affected = client
                .query_typed(&bq.sql, param_refs)
                .await
                .expect("INSERT ... RETURNING via SQLite session");

            // Step 4 — pull the BLOB back typed. The session's `query`
            // surface stringifies BLOBs as `<N bytes blob>` placeholders, so
            // we reach for the typed surface via the session handle's
            // `query_typed` helper.
            let typed = client
                .query_typed(
                    "SELECT id, ssn FROM \"app_demo\".\"users\" WHERE id = ?",
                    &[row_pk.into()],
                )
                .await
                .expect("SELECT typed");
            assert_eq!(typed.rows.len(), 1, "exactly one row must exist");
            let id_cell = &typed.rows[0][0];
            let ssn_cell = &typed.rows[0][1];
            let id_text = match id_cell {
                TypedCell::Text(s) => s.clone(),
                other => panic!("id must be TEXT, got {other:?}"),
            };
            assert_eq!(id_text, row_pk);
            let ssn_bytes = match ssn_cell {
                TypedCell::Blob(b) => b.clone(),
                other => panic!("ssn must be BLOB, got {other:?}"),
            };

            // Sanity — the stored bytes must equal the bytes the encryption
            // pass produced (base64-decoded back to raw). If the session's
            // sentinel strip / base64 decode is wrong, the stored bytes
            // diverge from the plaintext ciphertext.
            assert_eq!(
                ssn_bytes, ciphertext,
                "stored ciphertext must match the protection pass"
            );
            let mut row_value = crate::value!({
                "id": id_text,
                "ssn": crate::value::Value::Bytes(ssn_bytes),
            });

            decrypt_row_on_read(
                backend.key_store(),
                "app_demo",
                "users",
                &schema,
                &mut row_value,
            )
            .await
            .expect("decrypt_row_on_read");

            assert_eq!(
                row_value.get("ssn").and_then(|v| v.as_str()),
                Some(plaintext),
                "decrypted plaintext must recover: {row_value}",
            );
        });
    })
}
