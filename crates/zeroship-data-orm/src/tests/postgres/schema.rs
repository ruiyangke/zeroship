//! PostgreSQL schema contracts.
use super::fixtures::*;

use crate::tests::fixtures::Host;
use crate::tests::fixtures::schema::fixture_table_sql;

use crate::tests::fixtures::schema;

use zeroship_migrate::schema::query::FkEmission;

use compio_postgres::Pool;

use crate::value::{Value, value};

use crate::sql::compile::*;

/// Stamp a unique text `id` onto a seed insert document. The platform `id`
/// system field is `TEXT PRIMARY KEY` with NO DB default
/// -- production stamps a typed id via the system-fields pass before
/// `build_insert`. Tests that bypass that pass (calling `build_insert` directly)
/// must supply the `id` themselves, otherwise the row trips the `id` NOT-NULL.
fn with_seed_id(mut doc: Value) -> Value {
    use std::sync::atomic::{AtomicU64, Ordering};
    static N: AtomicU64 = AtomicU64::new(0);
    let n = N.fetch_add(1, Ordering::SeqCst);
    if let Some(obj) = doc.as_object_mut() {
        obj.entry("id".to_owned())
            .or_insert_with(|| Value::String(format!("seed_{n}")));
    }
    doc
}

#[test]
fn a1_unique_index_actually_enforces_uniqueness() {
    Host::test(|host| {
        host.run(async {
            let (_postgres, url) = require_pg(host).await;
            let pool = std::rc::Rc::new(Pool::connect(&url, 2).await.unwrap());

            // Fresh schema + table — `build_create_table` is the production path.
            let app = crate::tests::fixtures::test_app_id!();
            let app = app.as_str();
            let collection = "users";
            pool.execute(&format!("DROP SCHEMA IF EXISTS \"{app}\" CASCADE"), &[])
                .await
                .unwrap();
            pool.execute(
                &schema::fixture_schema_sql(
                    &crate::sql::SchemaName::new(app).expect("fixture schema name"),
                ),
                &[],
            )
            .await
            .unwrap();

            let schema = value!({
                "email": {"type": "string", "required": true, "unique": true},
                "handle": {"type": "string", "index": true},
            });

            let create_table = fixture_table_sql(
                &crate::sql::SchemaName::new(app).expect("fixture schema name"),
                collection,
                &schema,
                &FkEmission::Inline,
            )
            .unwrap();
            // `build_create_table_with_fks` emits MULTI-statement DDL (the CREATE TABLE
            // plus the system-field index `CREATE INDEX`s, and on PG the
            // `COMMENT ON COLUMN … 'zero-migrate:mask:…'` / `'zero-migrate:enc:…'` sentinels). The
            // extended/prepared `execute` path rejects that with `42601 cannot insert
            // multiple commands into a prepared statement`; the simple-query
            // `batch_execute` is the correct executor for rendered DDL batches.
            pool.batch_execute(&create_table).await.unwrap();

            // Generate and execute the new index DDL.
            let indexes = schema::fixture_indexes(
                &crate::sql::SchemaName::new(app).expect("fixture schema name"),
                collection,
                &schema,
            )
            .unwrap();
            assert_eq!(indexes.len(), 2, "expected 2 indexes, got: {indexes:?}");

            for spec in &indexes {
                pool.execute(&spec.sql, &[]).await.unwrap_or_else(|e| {
                    panic!("failed to run {}: {e}", spec.sql);
                });
            }

            // Look up pg_index entries on the new schema.
            let q = format!(
                "SELECT c.relname AS idx_name, i.indisunique, i.indisvalid
         FROM pg_index i
         JOIN pg_class c ON c.oid = i.indexrelid
         JOIN pg_namespace n ON n.oid = c.relnamespace
         WHERE n.nspname = '{app}'
         ORDER BY c.relname"
            );
            let rows = pool.query_text_params(&q, &[]).await.unwrap();
            // Two indexes (we don't count the PK; SERIAL PRIMARY KEY also makes an
            // index, so total is at least 3 — but we assert specifically on names).
            let names: Vec<(String, bool, bool)> = rows
                .iter()
                .map(|r| {
                    (
                        r.get::<_, String>("idx_name"),
                        r.get::<_, bool>("indisunique"),
                        r.get::<_, bool>("indisvalid"),
                    )
                })
                .collect();

            let email_key = names.iter().find(|(n, _, _)| n == "users_email_key");
            let handle_idx = names.iter().find(|(n, _, _)| n == "users_handle_idx");
            assert!(
                email_key.is_some(),
                "expected users_email_key, found: {names:?}"
            );
            assert!(
                handle_idx.is_some(),
                "expected users_handle_idx, found: {names:?}"
            );
            let (_, unique, valid) = email_key.unwrap();
            assert!(*unique, "users_email_key should be unique");
            assert!(*valid, "users_email_key should be valid");
            let (_, unique2, valid2) = handle_idx.unwrap();
            assert!(!*unique2, "users_handle_idx should NOT be unique");
            assert!(*valid2, "users_handle_idx should be valid");

            // -----------------------------------------------------------------------
            // The silent-bug live repro: insert two rows with the same email and
            // assert the second one fails with SQLSTATE 23505.
            // -----------------------------------------------------------------------
            let ins1 = build_insert(
                &crate::sql::SchemaName::new(app).expect("fixture schema name"),
                collection,
                &schema,
                &with_seed_id(value!({"email": "a@x.com"})),
            )
            .unwrap();
            let p1 = &ins1.params;
            zeroship_data_orm::backend::postgres::params::query(
                &pool.acquire().await.unwrap(),
                &ins1.sql,
                p1,
            )
            .await
            .unwrap();

            // Distinct `id` so the second insert is rejected for the DUPLICATE EMAIL
            // (the unique index under test), not an incidental duplicate PK.
            let ins2 = build_insert(
                &crate::sql::SchemaName::new(app).expect("fixture schema name"),
                collection,
                &schema,
                &with_seed_id(value!({"email": "a@x.com"})),
            )
            .unwrap();
            let p2 = &ins2.params;
            let err = zeroship_data_orm::backend::postgres::params::query(
                &pool.acquire().await.unwrap(),
                &ins2.sql,
                p2,
            )
            .await
            .unwrap_err();
            assert!(
                matches!(
                    err,
                    zeroship_data_orm::error::DbError::UniqueViolation { .. }
                ),
                "duplicate email must violate uniqueness: {err}"
            );

            // -----------------------------------------------------------------------
            // Idempotency — re-running build_create_indexes + executing the SQL
            // again must be a no-op (the IF NOT EXISTS + deterministic naming
            // contract).
            // -----------------------------------------------------------------------
            for spec in &indexes {
                pool.execute(&spec.sql, &[]).await.unwrap_or_else(|e| {
                    panic!("idempotent re-run failed for {}: {e}", spec.sql);
                });
            }
            release_pg(host, pool).await;
        })
    })
}

/// Helper: build `users` and `posts` where `posts.authorId` references `users`.
///
/// Raw SQL because plugin-db does not own DDL, so a test that wants tables has
/// to create them. The FK carries no `ON DELETE`
/// clause, which is what `t.ref` emits by default and what PostgreSQL records as
/// `confdeltype = 'a'` (NO ACTION) - the variants that need CASCADE or RESTRICT
/// spell their own.
async fn b2_setup_users_posts(pool: &std::rc::Rc<Pool>, app: &str) {
    pool.execute(&format!("DROP SCHEMA IF EXISTS \"{app}\" CASCADE"), &[])
        .await
        .unwrap();
    pool.batch_execute(&format!(
        r#"CREATE SCHEMA "{app}";
CREATE TABLE "{app}"."users" ({PG_SYSTEM_COLUMNS},
  "name" TEXT NOT NULL
);
{users_idx}
CREATE TABLE "{app}"."posts" ({PG_SYSTEM_COLUMNS},
  "title" TEXT NOT NULL,
  "authorId" TEXT,
  CONSTRAINT "authorId_fkey" FOREIGN KEY ("authorId") REFERENCES "{app}"."users" ("id")
);
{posts_idx}"#,
        users_idx = pg_system_indexes(app, "users"),
        posts_idx = pg_system_indexes(app, "posts"),
    ))
    .await
    .expect("b2 users + posts fixture");
}

#[test]
fn b2_ref_creates_foreign_key() {
    Host::test(|host| {
        host.run(async {
            let (_postgres, url) = require_pg(host).await;
            let pool = std::rc::Rc::new(Pool::connect(&url, 4).await.unwrap());

            let app = crate::tests::fixtures::test_app_id!();

            let app = app.as_str();
            b2_setup_users_posts(&pool, app).await;

            // Inspect pg_constraint for the FK on "posts.authorId".
            let rows = pool
                .query_text_params(
                    r#"
SELECT con.conname AS name,
       con.confdeltype::text AS on_delete,
       con.confupdtype::text AS on_update,
       con.condeferrable AS deferrable,
       fcl.relname AS target
  FROM pg_constraint con
  JOIN pg_class cl ON cl.oid = con.conrelid
  JOIN pg_class fcl ON fcl.oid = con.confrelid
  JOIN pg_namespace n ON n.oid = cl.relnamespace
 WHERE n.nspname = $1 AND cl.relname = 'posts' AND con.contype = 'f'
"#,
                    &[app],
                )
                .await
                .unwrap();
            assert_eq!(rows.len(), 1, "expected one FK on posts.authorId");
            let target: String = rows[0].get("target");
            assert_eq!(target, "users");
            // `t.ref()` emits NO REFERENTIAL ACTION AT ALL, so Postgres' own defaults
            // stand: NO ACTION on both sides ('a'), checked immediately rather than
            // deferred. That is the contract settled in docs/reference/db.md:362-364
            // ("the database's own defaults apply: NO ACTION for both actions, and
            // immediate (non-deferred) checking") and implemented at
            // crates/zeroship-data-orm/src/sql/compile.rs, which OMITS the ON DELETE
            // clause when the action is NO ACTION.
            //
            // These three assertions read `r`/`r`/`true` until 2026-08-12 -- the
            // RESTRICT-and-deferrable contract the project decided AGAINST. Nothing
            // caught it because this binary runs in no CI job at all (see the header).
            // Values below are MEASURED against a live FK, not copied from the doc;
            // measuring first was the point, because had they disagreed the
            // disagreement would have been a product finding rather than a stale test.
            let on_delete: String = rows[0].get("on_delete");
            assert_eq!(on_delete, "a", "expected NO ACTION, got {on_delete}");
            let on_update: String = rows[0].get("on_update");
            assert_eq!(on_update, "a", "expected NO ACTION, got {on_update}");
            let deferrable: bool = rows[0].get("deferrable");
            assert!(
                !deferrable,
                "expected an IMMEDIATE (non-deferrable) FK check"
            );
            release_pg(host, pool).await;
        })
    })
}

#[test]
fn b2_ref_blocks_orphan_insert() {
    Host::test(|host| {
        host.run(async {
            let (_postgres, url) = require_pg(host).await;
            let pool = std::rc::Rc::new(Pool::connect(&url, 4).await.unwrap());

            let app = crate::tests::fixtures::test_app_id!();

            let app = app.as_str();
            b2_setup_users_posts(&pool, app).await;

            // Insert into posts with non-existent authorId; must fail with FK violation.
            // `id` is `TEXT PRIMARY KEY` (no DB default) -- supply one
            // so the row reaches FK validation rather than tripping the id NOT NULL.
            let result = pool
        .query_text_params(
            &format!(
                "INSERT INTO \"{app}\".\"posts\" (\"id\", \"title\", \"authorId\") VALUES ($1, $2, $3)"
            ),
            &["pst_b2_orphan_1", "hello", "usr_does_not_exist"],
        )
        .await;
            let err = result.expect_err("orphan insert should fail");
            let err_str = format!("{err:?}");
            // SQLSTATE 23503 = foreign_key_violation
            assert!(
                err_str.contains("23503") || err_str.to_lowercase().contains("foreign key"),
                "expected foreign_key_violation, got: {err_str}"
            );
            release_pg(host, pool).await;
        })
    })
}

#[test]
fn b2_ref_on_delete_restrict_blocks_parent_delete() {
    Host::test(|host| {
        host.run(async {
            let (_postgres, url) = require_pg(host).await;
            let pool = std::rc::Rc::new(Pool::connect(&url, 4).await.unwrap());

            let app = crate::tests::fixtures::test_app_id!();

            let app = app.as_str();
            b2_setup_users_posts(&pool, app).await;

            // Insert one user + one post that references it. `id`
            // is `TEXT PRIMARY KEY` (no DB default -- production stamps a typed id via
            // the system-fields pass), so the seed INSERT must supply it and read it as
            // text. A `posts` row also needs its own `id`.
            let user_id = "usr_b2_restrict_1";
            let user_rows = pool
                .query_text_params(
                    &format!(
                "INSERT INTO \"{app}\".\"users\" (\"id\", \"name\") VALUES ($1, $2) RETURNING id"
            ),
                    &[user_id, "alice"],
                )
                .await
                .unwrap();
            let user_id: String = user_rows[0].get("id");
            pool.query_text_params(
                &format!(
            "INSERT INTO \"{app}\".\"posts\" (\"id\", \"title\", \"authorId\") VALUES ($1, $2, $3)"
        ),
                &["pst_b2_restrict_1", "hello", &user_id],
            )
            .await
            .unwrap();

            // Now try to delete the user — RESTRICT must refuse.
            let result = pool
                .query_text_params(
                    &format!("DELETE FROM \"{app}\".\"users\" WHERE id = $1"),
                    &[&user_id],
                )
                .await;
            let err = result.expect_err("RESTRICT must block parent delete");
            let err_str = format!("{err:?}");
            assert!(
                err_str.contains("23503") || err_str.to_lowercase().contains("foreign key"),
                "expected foreign_key_violation, got: {err_str}"
            );
            release_pg(host, pool).await;
        })
    })
}

#[test]
fn b2_ref_on_delete_cascade_deletes_children() {
    Host::test(|host| {
        host.run(async {
            let (_postgres, url) = require_pg(host).await;
            let pool = std::rc::Rc::new(Pool::connect(&url, 4).await.unwrap());

            let app = crate::tests::fixtures::test_app_id!();

            let app = app.as_str();
            pool.execute(&format!("DROP SCHEMA IF EXISTS \"{app}\" CASCADE"), &[])
                .await
                .unwrap();

            // Raw SQL, and the `ON DELETE CASCADE` is the point of the test - it is what
            // `"onDelete": "cascade"` on a `t.ref` emits, spelled here because the
            // migration service, not plugin-db, owns schema changes.
            pool.batch_execute(&format!(
                r#"CREATE SCHEMA "{app}";
CREATE TABLE "{app}"."users" ({PG_SYSTEM_COLUMNS},
  "name" TEXT NOT NULL
);
{users_idx}
CREATE TABLE "{app}"."posts" ({PG_SYSTEM_COLUMNS},
  "title" TEXT NOT NULL,
  "authorId" TEXT,
  CONSTRAINT "authorId_fkey" FOREIGN KEY ("authorId")
    REFERENCES "{app}"."users" ("id") ON DELETE CASCADE
);
{posts_idx}"#,
                users_idx = pg_system_indexes(app, "users"),
                posts_idx = pg_system_indexes(app, "posts"),
            ))
            .await
            .expect("cascade fixture");

            // Insert user + 3 posts that reference it. `id` is
            // `TEXT PRIMARY KEY` (no DB default), so seed inserts must supply text ids.
            let user_id = "usr_b2_cascade_1";
            let user_rows = pool
                .query_text_params(
                    &format!(
                "INSERT INTO \"{app}\".\"users\" (\"id\", \"name\") VALUES ($1, $2) RETURNING id"
            ),
                    &[user_id, "bob"],
                )
                .await
                .unwrap();
            let user_id: String = user_rows[0].get("id");
            for (i, title) in ["a", "b", "c"].iter().enumerate() {
                pool.query_text_params(
            &format!(
                "INSERT INTO \"{app}\".\"posts\" (\"id\", \"title\", \"authorId\") VALUES ($1, $2, $3)"
            ),
            &[&format!("pst_b2_cascade_{i}"), title, &user_id],
        )
        .await
        .unwrap();
            }

            // Delete the user — CASCADE should also delete the 3 posts.
            pool.query_text_params(
                &format!("DELETE FROM \"{app}\".\"users\" WHERE id = $1"),
                &[&user_id],
            )
            .await
            .unwrap();

            let count_rows = pool
                .query_text_params(
                    &format!("SELECT COUNT(*) AS n FROM \"{app}\".\"posts\""),
                    &[],
                )
                .await
                .unwrap();
            let n: i64 = count_rows[0].get("n");
            assert_eq!(n, 0, "CASCADE should have deleted all child posts");
            release_pg(host, pool).await;
        })
    })
}

#[test]
fn b2_circular_refs_via_deferrable() {
    Host::test(|host| {
        host.run(async {
            let (_postgres, url) = require_pg(host).await;
            let pool = std::rc::Rc::new(Pool::connect(&url, 4).await.unwrap());

            let app = crate::tests::fixtures::test_app_id!();

            let app = app.as_str();
            pool.execute(&format!("DROP SCHEMA IF EXISTS \"{app}\" CASCADE"), &[])
                .await
                .unwrap();

            // a → ref(b), b → ref(a). Order matters for first creation:
            // we register a then b. The FK from `a.bId → b.id` must be deferred
            // until b is created. The current `build_create_table` always emits
            // FK inline, so when registering `a` while `b` doesn't yet exist,
            // we'd fail. We therefore register `b` first (no refs), then `a`
            // (with FK to b), then ALTER b to add its FK to a.
            //
            // For this test we register both with FK clauses inline, but use
            // DEFERRABLE INITIALLY DEFERRED so the runtime can insert into
            // a + b within a single transaction in any order.
            //
            // The setup uses two separate calls; we drop the FK from `a.bId`
            // temporarily and re-add it after both tables exist to side-step
            // the cold-start ordering problem. The B2 implementation defers
            // truly inter-table FK creation to a follow-up; today we exercise
            // the DEFERRABLE behaviour by creating both tables, attaching the
            // FK, then verifying a single transaction can insert in any order.

            // Create the tables manually without FK, then add FKs.
            pool.execute(&format!("CREATE SCHEMA \"{app}\""), &[])
                .await
                .unwrap();
            pool.execute(
        &format!(
            "CREATE TABLE \"{app}\".\"a\" (id SERIAL PRIMARY KEY, b_id INTEGER, created_at TIMESTAMPTZ DEFAULT NOW())"
        ),
        &[],
    )
    .await
    .unwrap();
            pool.execute(
        &format!(
            "CREATE TABLE \"{app}\".\"b\" (id SERIAL PRIMARY KEY, a_id INTEGER, created_at TIMESTAMPTZ DEFAULT NOW())"
        ),
        &[],
    )
    .await
    .unwrap();
            // Add cyclic FKs as DEFERRABLE INITIALLY DEFERRED.
            pool.execute(
        &format!(
            "ALTER TABLE \"{app}\".\"a\" ADD CONSTRAINT a_b_fkey FOREIGN KEY (b_id) REFERENCES \"{app}\".\"b\"(id) DEFERRABLE INITIALLY DEFERRED"
        ),
        &[],
    )
    .await
    .unwrap();
            pool.execute(
        &format!(
            "ALTER TABLE \"{app}\".\"b\" ADD CONSTRAINT b_a_fkey FOREIGN KEY (a_id) REFERENCES \"{app}\".\"a\"(id) DEFERRABLE INITIALLY DEFERRED"
        ),
        &[],
    )
    .await
    .unwrap();

            // Verify both constraints are DEFERRABLE.
            let rows = pool
                .query_text_params(
                    r#"
SELECT con.conname AS name, con.condeferrable AS def, con.condeferred AS init_deferred
  FROM pg_constraint con
  JOIN pg_class cl ON cl.oid = con.conrelid
  JOIN pg_namespace n ON n.oid = cl.relnamespace
 WHERE n.nspname = $1 AND con.contype = 'f'
 ORDER BY con.conname
"#,
                    &[app],
                )
                .await
                .unwrap();
            assert_eq!(rows.len(), 2);
            for row in &rows {
                let def: bool = row.get("def");
                let init_deferred: bool = row.get("init_deferred");
                let name: String = row.get("name");
                assert!(def, "FK {name} must be DEFERRABLE");
                assert!(init_deferred, "FK {name} must be INITIALLY DEFERRED");
            }

            // Insert pair in a single transaction — order doesn't matter
            // because the FK check is deferred to COMMIT. We insert into `a`
            // referencing a `b` row that doesn't exist yet, then create the
            // `b` row referencing the `a` row, all within the tx.
            let client = pool.acquire().await.unwrap();
            client.execute("BEGIN", &[]).await.unwrap();
            client
                .execute(
                    &format!("INSERT INTO \"{app}\".\"a\" (id, b_id) VALUES (1, 1)"),
                    &[],
                )
                .await
                .unwrap();
            client
                .execute(
                    &format!("INSERT INTO \"{app}\".\"b\" (id, a_id) VALUES (1, 1)"),
                    &[],
                )
                .await
                .unwrap();
            client.execute("COMMIT", &[]).await.unwrap();

            // Confirm the rows exist.
            let count_rows = pool
        .query_text_params(
            &format!("SELECT (SELECT COUNT(*) FROM \"{app}\".\"a\") AS na, (SELECT COUNT(*) FROM \"{app}\".\"b\") AS nb"),
            &[],
        )
        .await
        .unwrap();
            let na: i64 = count_rows[0].get("na");
            let nb: i64 = count_rows[0].get("nb");
            assert_eq!(na, 1);
            assert_eq!(nb, 1);
            drop(client);
            release_pg(host, pool).await;
        })
    })
}

/// Count every relation `pg_class` holds in the app's schema -- tables, indexes,
/// sequences, views, virtual and partitioned relations alike -- or `None` when
/// the schema itself does not exist.
///
/// This REPLACED an `audit_row_count` probe that counted rows in
/// `"<app>"."__zeroship_migrations"`, deleted along with the data-plane DDL it
/// was the provenance log for. The replacement is deliberately a WIDER
/// instrument, not a like-for-like one: the old probe could only see DDL that
/// chose to write an audit row, so a runtime `CREATE INDEX` that skipped the
/// audit write was invisible to it. This one is keyed on the catalog, so any
/// relation the dispatch creates moves the number whether or not the code that
/// created it wanted to be seen.
///
/// What it still cannot see: DDL that creates no relation at all -- `ALTER
/// TABLE ... ADD COLUMN`, `COMMENT ON`, `GRANT`, a `CREATE TRIGGER`. The two
/// callers below pair it with an explicit relation-existence assertion for the
/// object each is actually about.
async fn schema_relation_count(pool: &std::rc::Rc<Pool>, app: &str) -> Option<i64> {
    let rows = pool
        .query_text_params(
            "SELECT count(c.oid)::bigint AS n \
             FROM pg_namespace n \
             LEFT JOIN pg_class c ON c.relnamespace = n.oid \
             WHERE n.nspname = $1 \
             GROUP BY n.oid",
            &[app],
        )
        .await
        .ok()?;
    // No row at all means the namespace is absent -- distinct from a namespace
    // that exists and holds nothing, which returns 0.
    Some(rows.first()?.get::<_, i64>("n"))
}

/// Descriptor-driven CRUD with no runtime DDL. The engine creates
/// the schema at deploy (here simulated by the same DDL the relocated engine
/// emits). Encryption + mask CRUD then round-trip end-to-end from the runtime
/// descriptor while the catalog relation count stays unchanged.
///
/// The `zero-migrate:enc` / `zero-migrate:mask` column-comment sentinels this fixture used to plant
/// are gone with the catalog read that recovered them; see
/// `p4_round_trip_encrypted_masked_vector_via_descriptor_metadata` for the full
/// reasoning. The round trip below is unchanged.
#[test]
fn p5_pg_crud_works_via_engine_created_schema_without_runtime_ddl() {
    Host::test(|host| {
        host.run(async {
            let (_postgres, url) = require_pg(host).await;
            let pool = std::rc::Rc::new(Pool::connect(&url, 4).await.unwrap());

            let app = crate::tests::fixtures::test_app_id!();

            let app = app.as_str();
            let _keys = host.supply_project_key(&[app], &"e".repeat(64));
            pool.execute(&format!("DROP SCHEMA IF EXISTS \"{app}\" CASCADE"), &[])
                .await
                .unwrap();

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
            });

            // === Simulate the engine/deploy-apply: create the table. ===
            // The SAME DDL shape the relocated engine emits, post-flip: `phone`
            // (masked, not encrypted) carries the bare-TEXT mask in its own column,
            // and its raw sibling (`raw_column_name`) carries the real value. `ssn` is
            // encrypted-only, so it is NOT flipped -- unchanged BYTEA in its own slot.
            let phone_raw = raw_column_name("phone");
            pool.batch_execute(&format!(
                r#"CREATE SCHEMA IF NOT EXISTS "{app}";
CREATE TABLE "{app}"."people" ({PG_SYSTEM_COLUMNS},
  "name" TEXT NOT NULL,
  "ssn" BYTEA,
  "phone" TEXT,
  "{phone_raw}" TEXT
);
{idx}"#,
                idx = pg_system_indexes(app, "people"),
            ))
            .await
            .unwrap_or_else(|e| panic!("people fixture (deploy stand-in) failed: {e}"));

            // Snapshot the catalog after the engine stand-in's apply. Serving CRUD
            // below must not add a relation to it.
            let relations_before = schema_relation_count(&pool, app).await;
            assert!(
                relations_before.unwrap_or(0) > 0,
                "engine stand-in must have created relations to compare against, got \
         {relations_before:?}"
            );

            // Install the runtime backend and the descriptor entry the runtime plants
            // natively at boot.
            host.install_postgres_pool(std::rc::Rc::clone(&pool), &url);
            crate::tests::fixtures::cache_schema(app, "people", schema.clone());

            // The resolution the CRUD passes will perform returns BOTH goodies.
            let resolved = zeroship_data_orm::crud::runtime_schema_for_tests(app, "people")
                .expect("the descriptor entry this deploy installed must resolve");
            assert_eq!(resolved["phone"]["mask"]["kind"], "last4");

            // ----- WRITE via the real pipeline (descriptor metadata) -----
            // No `id`: the write pipeline refuses a creator-supplied one and mints a
            // typed id. The raw INSERT below MUST carry that minted id - `ssn` is a
            // `randomised` encrypted column, so the row primary key is bound into the
            // AEAD's additional data on write and reconstructed from the row's `id` on
            // read. A literal id here relocates the ciphertext onto another row, and
            // the read correctly refuses it with `encryption_aead_failed`.
            let mut docs = value!([{
                "name": "Grace",
                "ssn": "987-65-4321",
                "phone": "650-555-0199",
            }]);
            host.prepare_insert_many_docs(&mut docs, app, "people", None)
                .await
                .expect("write pipeline");
            let doc = &docs[0];
            let row_id = doc["id"]
                .as_str()
                .expect("the write pipeline mints the row id, and the AAD binds it")
                .to_string();
            assert!(
                doc["ssn"].as_bytes().is_some(),
                "ssn must be ciphertext on write, got {:?}",
                doc["ssn"]
            );
            assert_eq!(
                doc["phone"],
                value!("***-***-0199"),
                "mask pass must move the last4 mask into phone's own column on write, got {:?}",
                doc["phone"]
            );
            assert_ne!(
                doc["phone"],
                value!("650-555-0199"),
                "phone's own column must not carry the real value after relocation, got {:?}",
                doc["phone"]
            );
            assert_eq!(
                doc[phone_raw.as_str()],
                value!("650-555-0199"),
                "the real phone value must be relocated to the raw sibling column, got {:?}",
                doc[phone_raw.as_str()]
            );

            let ciphertext = doc["ssn"].as_bytes().unwrap();
            let phone_mask = doc["phone"].as_str().unwrap().to_string();
            let phone_real = doc[phone_raw.as_str()].as_str().unwrap().to_string();
            pool.execute(
                &format!(
                    "INSERT INTO \"{app}\".\"people\" (id, name, ssn, phone, \"{phone_raw}\") \
             VALUES ($1, $2, $3::bytea, $4, $5)"
                ),
                &[
                    &row_id.as_str(),
                    &"Grace",
                    &ciphertext,
                    &phone_mask.as_str(),
                    &phone_real.as_str(),
                ],
            )
            .await
            .unwrap();

            // ----- READ via the real pipeline (introspected metadata) -----
            // `phone` is read directly -- it already holds the mask after the storage
            // flip, so no alias is needed the way `phone_masked AS phone` used to be.
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
            assert_eq!(
                out["ssn"],
                value!("987-65-4321"),
                "encrypted column decrypts to plaintext on read, got {:?}",
                out["ssn"]
            );
            assert_eq!(
                out["phone"]["sentinel"],
                value!("__zsmask__"),
                "phone wrapped"
            );
            assert_eq!(out["phone"]["masked"], value!("***-***-0199"));
            assert_eq!(out["phone"]["classification"], value!("pci"));
            assert!(
                !out.to_string().contains("650-555-0199"),
                "the real phone number must not appear anywhere in the finalized row, got {out:?}"
            );

            // FINAL proof: still zero runtime DDL after the full CRUD round-trip.
            assert_eq!(
                schema_relation_count(&pool, app).await,
                relations_before,
                "P5 PG cutover: CRUD must not have triggered any relation-creating DDL"
            );

            let _ = pool
                .execute(&format!("DROP SCHEMA IF EXISTS \"{app}\" CASCADE"), &[])
                .await;
            release_pg(host, pool).await;
        })
    })
}
