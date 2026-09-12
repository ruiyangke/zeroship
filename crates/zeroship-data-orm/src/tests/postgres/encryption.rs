//! PostgreSQL encryption contracts.
use super::fixtures::*;

use crate::tests::fixtures::Host;

use compio_postgres::Pool;

use crate::value::{value, Value};

use crate::sql::mapping::*;

use zeroship_data_orm::error::DbError;

use zeroship_data_orm::encryption;

/// Gate #1: round-trip an encrypted string column. Insert a
/// row with `ssn` declared `t.encrypted({  })`,
/// read it back via the PG path, expect the plaintext to recover.
#[test]
fn encrypted_column_round_trip_randomised() {
    Host::test(|host| {
        host.run(async {
            let (_postgres, url) = require_pg(host).await;
            let schema = crate::tests::fixtures::test_app_id!();
            let schema = schema.as_str();
            let pool = std::rc::Rc::new(Pool::connect(&url, 2).await.unwrap());
            // Synthetic 32-byte root key.
            let _keys = host.supply_project_key(&["app1"], &"a".repeat(64));

            pool.execute(&format!("DROP SCHEMA IF EXISTS \"{schema}\" CASCADE"), &[])
                .await
                .unwrap();
            pool.execute(&format!("CREATE SCHEMA \"{schema}\""), &[])
                .await
                .unwrap();
            // Manually create the table; the encryption pass operates on generic
            // BYTEA columns regardless of which migration emitted them.
            pool.execute(
                &format!(
                    r#"CREATE TABLE "{schema}"."enc_notes" (
                id   TEXT PRIMARY KEY,
                ssn  BYTEA
            )"#
                ),
                &[],
            )
            .await
            .unwrap();

            let backend = zeroship_data_orm::backend::PostgresBackend::new(
                pool.clone(),
                url.clone(),
                host.key_source(),
            );
            let key = backend
                .key_store()
                .resolve("app1")
                .await
                .expect("resolve_key");
            let plaintext = b"123-45-6789";
            let aad = encryption::canonical_aad("app1", "enc_notes", "ssn", b"row_a");
            let ct = zeroship_data_orm::encryption::aead::encrypt(&key, plaintext, &aad)
                .expect("encrypt");

            // Bind via base64 decode just like the build_insert layer does.
            let b64 = base64::Engine::encode(&base64::engine::general_purpose::STANDARD, &ct);
            pool.execute(
        &format!(
            "INSERT INTO \"{schema}\".\"enc_notes\" (id, ssn) VALUES ($1, decode($2, 'base64')::bytea)"
        ),
        &[&"row_a", &b64.as_str()],
    )
    .await
    .unwrap();

            // Read back as BYTEA via `encode(ssn, 'hex')` so the text protocol
            // surfaces a hex string we can parse cleanly. (Reading the BYTEA
            // column directly via Row::get<String> fails because the
            // text-format BYTEA representation isn't UTF-8 in general.)
            let rows = pool
        .query_text_params(
            &format!(
                "SELECT encode(ssn, 'hex') AS ssn_hex FROM \"{schema}\".\"enc_notes\" WHERE id = $1"
            ),
            &["row_a"],
        )
        .await
        .unwrap();
            let hex_str: String = rows[0].get("ssn_hex");
            let raw = {
                let mut out = Vec::with_capacity(hex_str.len() / 2);
                for chunk in hex_str.as_bytes().chunks(2) {
                    let pair = std::str::from_utf8(chunk).unwrap();
                    out.push(u8::from_str_radix(pair, 16).unwrap());
                }
                out
            };
            let recovered =
                zeroship_data_orm::encryption::aead::decrypt(&key, &raw, &aad).expect("decrypt");
            assert_eq!(recovered, plaintext);
            drop(backend);
            release_pg(host, pool).await;
        })
    })
}

/// Camp A fence: copying ciphertext from row A into row
/// B's slot must surface `encryption_aead_failed` (row_pk in AAD
/// defeats the ciphertext-oracle attack on randomised columns).
#[test]
fn encrypted_randomised_row_swap_rejected() {
    Host::test(|host| {
        host.run(async {
            let (_postgres, url) = require_pg(host).await;
            let schema = crate::tests::fixtures::test_app_id!();
            let schema = schema.as_str();
            let pool = std::rc::Rc::new(Pool::connect(&url, 2).await.unwrap());
            let _keys = host.supply_project_key(&["app1"], &"b".repeat(64));

            pool.execute(&format!("DROP SCHEMA IF EXISTS \"{schema}\" CASCADE"), &[])
                .await
                .unwrap();
            pool.execute(&format!("CREATE SCHEMA \"{schema}\""), &[])
                .await
                .unwrap();
            pool.execute(
                &format!(
                    r#"CREATE TABLE "{schema}"."enc_notes" (
                id   TEXT PRIMARY KEY,
                ssn  BYTEA
            )"#
                ),
                &[],
            )
            .await
            .unwrap();

            let backend = zeroship_data_orm::backend::PostgresBackend::new(
                pool.clone(),
                url.clone(),
                host.key_source(),
            );
            let key = backend.key_store().resolve("app1").await.unwrap();
            // Insert row A with its OWN AAD (binds row_pk = "row_a").
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
                let b64 = base64::Engine::encode(&base64::engine::general_purpose::STANDARD, ct);
                pool.execute(
            &format!(
                "INSERT INTO \"{schema}\".\"enc_notes\" (id, ssn) VALUES ($1, decode($2, 'base64')::bytea)"
            ),
            &[&id, &b64.as_str()],
        )
        .await
        .unwrap();
            }

            // Attacker move: copy row A's ciphertext into row B's slot.
            let b64_a = base64::Engine::encode(&base64::engine::general_purpose::STANDARD, &ct_a);
            pool.execute(
        &format!(
            "UPDATE \"{schema}\".\"enc_notes\" SET ssn = decode($1, 'base64')::bytea WHERE id = $2"
        ),
        &[&b64_a.as_str(), &"row_b"],
    )
    .await
    .unwrap();

            // Read row B → decrypt with row B's AAD (row_pk = "row_b"). Use
            // `encode(ssn, 'hex')` per the round-trip test above.
            let rows = pool
        .query_text_params(
            &format!(
                "SELECT encode(ssn, 'hex') AS ssn_hex FROM \"{schema}\".\"enc_notes\" WHERE id = $1"
            ),
            &["row_b"],
        )
        .await
        .unwrap();
            let hex_str: String = rows[0].get("ssn_hex");
            let raw = {
                let mut out = Vec::with_capacity(hex_str.len() / 2);
                for chunk in hex_str.as_bytes().chunks(2) {
                    let pair = std::str::from_utf8(chunk).unwrap();
                    out.push(u8::from_str_radix(pair, 16).unwrap());
                }
                out
            };
            let aad_b = encryption::canonical_aad("app1", "enc_notes", "ssn", b"row_b");
            let err = zeroship_data_orm::encryption::aead::decrypt(&key, &raw, &aad_b)
                .expect_err("row-swap must fail AAD verification");
            match err {
                DbError::ValidationFailed { code, .. } => {
                    assert_eq!(code, "encryption_aead_failed");
                }
                other => panic!("expected ValidationFailed encryption_aead_failed, got {other:?}"),
            }
            drop(backend);
            release_pg(host, pool).await;
        })
    })
}

/// Round-trip e2e proof that schema creation and CRUD cohere: a collection
/// with an `encrypted` + a `masked` + a `vector` field, table created the way
/// the migration engine creates it, then CRUD driven ENTIRELY by the RUNTIME
/// DESCRIPTOR:
///   - insert through the REAL write pipeline -> AEAD-encrypts the encrypted
///     column and populates the masked display value;
///   - read raw rows back, finalize through the REAL read pipeline -> decrypts
///     the encrypted column to plaintext and wraps the masked column.
///
/// **The metadata source changed and the round trip did not.** This test used to
/// plant `COMMENT ON COLUMN ... 'zero-migrate:enc:...'` / `'zero-migrate:mask:...'` sentinels and
/// assert the data plane RECOVERED the encryption mode and mask kind from the
/// live catalog. That recovery is deleted: the sentinels were emitted by the
/// migration engine out of the same DSL the descriptor is folded from, so the
/// catalog could only ever agree with the descriptor or be stale, and the read
/// cost one whole-schema catalog walk per cold collection. The metadata is now
/// installed by `cache_schema` from a descriptor-shaped field map,
/// matching the native runtime descriptor hook. Everything after that line is unchanged,
/// so what this still proves is what it always mattered for: the encrypt/mask
/// write stages and the decrypt/mask-wrap read stages agree, against a real
/// Postgres table, end to end.
#[test]
fn p4_round_trip_encrypted_masked_vector_via_descriptor_metadata() {
    Host::test(|host| {
        host.run(async {
            let (_postgres, url) = require_pg(host).await;
            let pool = std::rc::Rc::new(Pool::connect(&url, 4).await.unwrap());

            let app = crate::tests::fixtures::test_app_id!();

            let app = app.as_str();
            let _keys = host.supply_project_key(&[app], &"d".repeat(64));
            pool.execute(&format!("DROP SCHEMA IF EXISTS \"{app}\" CASCADE"), &[])
                .await
                .unwrap();

            // Schema: encrypted `ssn`, masked `phone`, and a `vector` embedding.
            let schema = value!({
                "name": {"type": "string", "required": true},
                "ssn": {
                    "type": "string",
                    "encrypted": true
                },
                "phone": {
                    "type": "string",
                    "mask": {"kind": "last4", "classification": "pci"}
                },
                "embedding": {"type": "vector", "vectorDims": 3, "vectorMetric": "cosine"},
            });

            // The table the migration engine would have created, sibling column
            // included. No `COMMENT ON COLUMN` sentinels: nothing reads them any more.
            //
            // ONE THING HERE IS NOT A FAITHFUL REPRODUCTION, and it is called out rather
            // than hidden: `embedding` gets a bare `vector(3)` column and NO ANN index.
            // The declared schema asks for a cosine vector index, and the engine DOES
            // emit one -- `vector_index_snapshot` in
            // `zeroship-migrate-core/src/render/declarative.rs:2888` renders
            // `USING ivfflat ("embedding" vector_cosine_ops) WITH (lists = 100)`. This
            // fixture just does not reproduce it, because it hand-writes the DDL rather
            // than running the engine.
            //
            // (That reason REPLACED an older one which said the index "is built by the
            // pgvector adapter in the backend, not by the shared emitter". That was true
            // of `VectorIndex::ensure_vector_index`, which is deleted: the data plane
            // issues no DDL at all now. The absence here is a fixture shortcut, not a
            // property of the system.)
            //
            // The column and its dimensionality are faithful; the index is absent. If a
            // future assertion here depends on the ANN index existing, it will fail, and
            // that failure is correct.
            // Post-flip: `phone` (masked, not encrypted) carries the bare-TEXT mask in
            // its own column; the real value sits in its raw sibling
            // (`raw_column_name`), typed the way the declared field would be. `ssn` is
            // encrypted-only (no `.mask()`), so it is NOT flipped -- its own column
            // keeps holding ciphertext, unchanged.
            let phone_raw = raw_column_name("phone");
            pool.batch_execute(&format!(
                r#"CREATE SCHEMA IF NOT EXISTS "{app}";
CREATE EXTENSION IF NOT EXISTS vector;
CREATE TABLE "{app}"."people" ({PG_COMMON_FIXTURE_COLUMNS},
  "name" TEXT NOT NULL,
  "ssn" BYTEA,
  "phone" TEXT,
  "{phone_raw}" TEXT,
  "embedding" vector(3)
);
{idx}"#,
                idx = pg_system_indexes(app, "people"),
            ))
            .await
            .unwrap_or_else(|e| panic!("p4 people fixture failed: {e}"));

            // Install the pool into the per-isolate context so the pipelines' own SQL
            // lands on this database, and install the DESCRIPTOR ENTRY the deploy would
            // have installed — exactly what the production register path does.
            host.install_postgres_pool(std::rc::Rc::clone(&pool), &url);
            crate::tests::fixtures::cache_schema(app, "people", schema.clone());

            // Sanity: the resolution the CRUD passes will perform returns BOTH goodies.
            let resolved = zeroship_data_orm::crud::runtime_schema_for_tests(app, "people")
                .expect("the descriptor entry this deploy installed must resolve");
            assert_eq!(resolved["phone"]["mask"]["kind"], "last4");

            // ----- WRITE (real pipeline, introspected metadata) -----
            // No `id`: the write pipeline refuses a creator-supplied one and mints a
            // typed id in `assignment_pass`. The raw INSERT below MUST then carry
            // THAT MINTED ID and nothing else: encryption binds the row primary key
            // into the AEAD's
            // additional data (`canonical_aad(collection, column, row_pk_bytes)` in
            // zeroship-data-orm's `encryption::aad`, stamped on write by
            // `protection::encryption_pass` and reconstructed on read from the row's `id`).
            // Storing this ciphertext under a DIFFERENT id and reading it back is a
            // ciphertext-relocation attack, and the AEAD refuses it with
            // `encryption_aead_failed` - correctly. Hard-coding a literal here is what
            // broke the test.
            let mut docs = value!([{
                "name": "Ada",
                "ssn": "123-45-6789",
                "phone": "415-555-0142",
                "embedding": [0.1, 0.2, 0.3],
            }]);
            host.prepare_insert_many_docs(&mut docs, app, "people", None)
                .await
                .expect("write pipeline");

            // The write pipeline encrypted `ssn` into native bytes
            // and RELOCATED `phone`: the mask moves into the field's OWN column and
            // the real value moves out to the raw sibling
            // (`mask_pass::relocate_masked_columns`).
            let doc = &docs[0];
            let row_id = doc["id"]
                .as_str()
                .expect("the write pipeline mints the row id, and the AAD binds it")
                .to_string();
            assert!(
                doc["ssn"].as_bytes().is_some(),
                "ssn must be replaced by ciphertext on write, got {:?}",
                doc["ssn"]
            );
            assert_eq!(
                doc["phone"],
                value!("***-***-0142"),
                "mask pass must move the last4 mask into phone's own column on write, got {:?}",
                doc["phone"]
            );
            assert_ne!(
                doc["phone"],
                value!("415-555-0142"),
                "phone's own column must not carry the real value after relocation, got {:?}",
                doc["phone"]
            );
            assert_eq!(
                doc[phone_raw.as_str()],
                value!("415-555-0142"),
                "the real phone value must be relocated to the raw sibling column, got {:?}",
                doc[phone_raw.as_str()]
            );

            // Persist it the way the SQL builder would (bind the encrypted bytes,
            // store the mask under `phone` and the real value under its raw sibling).
            let ciphertext = doc["ssn"].as_bytes().unwrap();
            let phone_mask = doc["phone"].as_str().unwrap().to_string();
            let phone_real = doc[phone_raw.as_str()].as_str().unwrap().to_string();
            // The vector literal is a test-controlled constant — format it inline with a
            // `::vector` cast (compio-postgres infers a `vector`-typed param from the
            // bind otherwise, which it cannot encode an `&str` into).
            pool.execute(
                &format!(
            "INSERT INTO \"{app}\".\"people\" (id, name, ssn, phone, \"{phone_raw}\", embedding) \
             VALUES ($1, $2, $3::bytea, $4, $5, '[0.1,0.2,0.3]'::vector)"
        ),
                &[
                    &row_id.as_str(),
                    &"Ada",
                    &ciphertext,
                    &phone_mask.as_str(),
                    &phone_real.as_str(),
                ],
            )
            .await
            .unwrap();

            // ----- READ (real pipeline, introspected metadata) -----
            // Fetch the raw row the way the SELECT builder would: the encrypted blob
            // as bytes, and `phone` read directly. Reads no longer alias anything
            // after the storage flip -- the field's own column already holds the mask.
            let raw = pool
                .query_text_params(
                    &format!(
                        "SELECT id, name, ssn, phone \
                 FROM \"{app}\".\"people\" WHERE id = $1"
                    ),
                    &[row_id.as_str()],
                )
                .await
                .unwrap();
            assert_eq!(raw.len(), 1);
            let row =
                zeroship_data_orm::backend::postgres::pg_row_json::row_to_value(&raw[0]).unwrap();

            let finalized = host
                .finalize_rows_on_read(app, "people", vec![row])
                .await
                .expect("read pipeline");
            let out = &finalized[0];

            // Encrypted column decrypted back to plaintext (driven by introspected meta).
            assert_eq!(
                out["ssn"],
                value!("123-45-6789"),
                "encrypted column must decrypt to plaintext on read, got {:?}",
                out["ssn"]
            );
            // Masked column wrapped into the platform MaskedValue sentinel, carrying the
            // last4-masked string + the introspected classification.
            assert_eq!(
                out["phone"]["sentinel"],
                value!("__zsmask__"),
                "phone wrapped"
            );
            assert_eq!(
                out["phone"]["masked"],
                value!("***-***-0142"),
                "masked phone surfaces last4 form, got {:?}",
                out["phone"]
            );
            assert_eq!(out["phone"]["classification"], value!("pci"));
            assert!(
                !out.to_string().contains("415-555-0142"),
                "the real phone number must not appear anywhere in the finalized row, got {out:?}"
            );
            release_pg(host, pool).await;
        })
    })
}

/// When no root key is configured for `missing_test` (the fallback
/// source holds none, and the PG getter resolves nothing because
/// nothing installs it), the PG resolver surfaces a typed
/// `column_key_not_configured` Configuration error rather than panicking
/// or returning Internal.
#[test]
fn encrypted_column_missing_key_typed_error() {
    Host::test(|host| {
        host.run(async {
            let (_postgres, url) = require_pg(host).await;
            let pool = std::rc::Rc::new(Pool::connect(&url, 2).await.unwrap());
            // Resolve against a source that provably has NO key: an empty
            // supplied set, so the fixture is independent of process configuration.
            let _keys = host.supply_project_key(&[], &"00".repeat(32));

            let backend = zeroship_data_orm::backend::PostgresBackend::new(
                pool.clone(),
                url.clone(),
                host.key_source(),
            );
            let err = backend
                .key_store()
                .resolve("app1")
                .await
                .expect_err("missing key must yield a typed error");
            match err {
                DbError::Configuration { code, .. } => {
                    assert_eq!(code, "column_key_not_configured");
                }
                other => panic!("expected Configuration column_key_not_configured, got {other:?}"),
            }
            drop(backend);
            release_pg(host, pool).await;
        })
    })
}

#[test]
fn pg_bytea_decoder_preserves_raw_binary_prefix_bytes() {
    Host::test(|host| {
        host.run(async {
            let (_postgres, url) = require_pg(host).await;
            let pool = std::rc::Rc::new(Pool::connect(&url, 2).await.unwrap());
            let rows = pool
                .query_text_params(
                    "SELECT decode('5c783431343234333434', 'hex')::bytea AS payload",
                    &[],
                )
                .await
                .unwrap();
            let json =
                zeroship_data_orm::backend::postgres::pg_row_json::row_to_value(&rows[0]).unwrap();
            let payload = json
                .get("payload")
                .and_then(Value::as_bytes)
                .expect("native payload bytes");
            let expected_raw = br"\x41424344".as_slice();
            let wrong_hex_decoded = b"ABCD".as_slice();

            assert_eq!(
                payload, expected_raw,
                "BYTEA decoding must preserve the raw binary wire bytes",
            );
            assert_ne!(
                payload, wrong_hex_decoded,
                "BYTEA decoding must not reinterpret raw binary bytes as a \
         text-protocol \\x... payload",
            );
            release_pg(host, pool).await;
        })
    })
}
