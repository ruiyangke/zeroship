//! **P7 — the schema-authority capstone.** One end-to-end test that chains the
//! REAL component functions of every phase against a real Postgres, proving the
//! whole pipeline coheres:
//!
//! ```text
//!  schema.js                                                          [INPUT]
//!     │  P3  zeroship_migrate_js::generate_migration
//!     │      (eval in V8 sandbox → descriptor IR → DeclarativeAuthor::diff
//!     │       → render dbmate)                              ← REAL fn
//!     ▼
//!  <ts>_<slug>.sql   (versioned migration; vector(N) + bytea + /* zsenc */ +
//!                     zsenc COMMENT + _masked sibling + __zsmask COMMENT + FK)
//!     │  P6  zeroship_bundle: Manifest.migrations[] + content-addressed blob,
//!     │      pack .zship (tar+zstd) → zeroship_bundle::ingest → blob store,
//!     │      read blob back by hash → reconstruct on disk
//!     │      (mirrors control's reconstruct_migration_files)   ← REAL fns
//!     ▼
//!  zeroship_control::deploy_migrate::apply_bundle_migrations  ← REAL fn
//!     │      (provision schema "<app_id>" + migrator_<app_id> role,
//!     │       load_dir → engine.plan(Confined) → engine.apply)
//!     ▼
//!  live DB: schema "<app_id>" with the goodie columns + sentinels
//!     │  P4/P5 zeroship_plugin_db: registerModel dispatch issues NO DDL,
//!     │      runtime_schema_for introspects live catalog + sentinels,
//!     │      write pipeline encrypts + masks, read pipeline decrypts + wraps,
//!     │      vector column accepts a vector                    ← REAL fns
//!     ▼
//!  CRUD round-trips byte-correct, driven ENTIRELY by introspected metadata,
//!  and the engine's journal is UNCHANGED by the data plane (sole-applier proof).
//! ```
//!
//! ## Where this runs
//!
//! Unlike the per-phase suites (migrate/control on `zeroship_migrate_test` :5440,
//! plugin-db on :5434), the CAPSTONE must run the whole pipeline against ONE DB
//! in ONE schema — deploy-apply and CRUD have to touch the same live schema for
//! the seams to actually cohere. The encrypted/masked/**vector** stage needs the
//! `vector` extension, so the capstone targets the pgvector instance on **:5434**
//! (`PG_TEST_URL`, default `postgres://postgres:test@localhost:5434/postgres`).
//! It also needs the `__zeroship_admin` bootstrap (the encryption key getter);
//! the test installs it via `auth::ensure_admin_schema`, and the actual root key
//! comes from `ZEROSHIP_COLUMN_KEY_DEFAULT` (the getter returns NULL → env
//! fallback, faithful to a pre-provisioned-key dev cluster).
//!
//! ## What is NOT driven here (documented human/CI boundary)
//!
//! This test drives the REAL component FUNCTIONS cross-crate. It does NOT boot
//! the full Docker stack — i.e. it does not exercise the control-plane ntex HTTP
//! deploy ENDPOINT (the `POST /apps/{id}/deploy` handler `run_deploy_migrations`
//! wraps `apply_bundle_migrations` in) nor the worker runtime DISPATCH over V8.
//! Those wrappers are a thin AppState/HTTP layer around the functions exercised
//! here; verifying them end-to-end requires `docker compose up` + a live gateway,
//! which is offline-blocked in this environment. That layer is verified by
//! `tests/e2e_platform.sh` / CI. See the report for the exact boundary.

use std::path::{Path, PathBuf};
use std::rc::Rc;

use compio_postgres::{Client, NoTls, Pool};
use uuid::Uuid;

// ---------------------------------------------------------------------------
// The INPUT: a creator schema.js exercising all four goodies the spec names —
// an encrypted field (auto-masked), an explicit masked field, a vector field,
// and a FK (t.ref). Self-contained JS: the eval graph resolves `@zeroship/db`
// to the embedded dist. (This is the front-end INPUT, not a component — the
// components are the functions it flows through.)
// ---------------------------------------------------------------------------
const SCHEMA_JS: &str = r#"
import { t } from "@zeroship/db";

const users = {
  email: t.string().required().unique(),
  // Encrypted (deterministic so .unique() stays coherent); auto-masks PII.
  ssn: t.encrypted({ mode: "deterministic", keyId: "default" }),
  // Explicit mask on a plain string.
  phone: t.string().mask({ kind: "last4", classification: "pci" }),
  age: t.number(),
};

const docs = {
  title: t.string().required(),
  // 3-dim cosine vector (small so the test can build the literal trivially;
  // the engine renders vector(N) for any N).
  embedding: t.vector(3, { metric: "cosine" }),
  // T12 full-text search: `.fts()` folds into a `__fts` GENERATED tsvector
  // column + a `docs__fts_idx` GIN index, emitted DECLARATIVELY by the engine
  // (tsvector + GIN are core Postgres — no extension needed). Pre-T12 this was
  // stripped at the IR boundary and produced a plain text column with no index.
  body: t.string().fts(),
  // T13 geoPoint: maps to a PostGIS `geography(POINT, 4326)` column + a
  // `docs_location_idx` GiST index, emitted DECLARATIVELY by the engine
  // (mirroring plugin-db's runtime `ensure_spatial_index`). Needs the PostGIS
  // extension on the capstone instance (alongside pgvector).
  location: t.geoPoint(),
  // FK to users with cascade delete.
  authorId: t.ref("users", { onDelete: "cascade" }),
};

export default { schema: { users, docs } };
"#;

const DEFAULT_PG_URL: &str = "postgres://postgres:test@localhost:5434/postgres";

fn pg_url() -> String {
    std::env::var("PG_TEST_URL").unwrap_or_else(|_| DEFAULT_PG_URL.to_string())
}

/// Open a raw admin client, or `None` when :5434 is unreachable (⇒ skip).
async fn admin_client() -> Option<Client> {
    match compio_postgres::connect(&pg_url(), NoTls).await {
        Ok((client, conn)) => {
            compio::runtime::spawn(async move {
                let _ = conn.run().await;
            })
            .detach();
            Some(client)
        }
        Err(e) => {
            eprintln!("SKIP: pgvector :5434 unreachable ({e})");
            None
        }
    }
}

/// Drop a per-app schema + meta schema + migrator role for a clean (re-)run.
async fn cleanup_app(conn: &Client, app_id: &Uuid) {
    let schema = app_id.to_string();
    let role = zeroship_migrate::migrator_role_name(&schema).unwrap();
    let q = |s: &str| format!("\"{}\"", s.replace('"', "\"\""));
    let _ = conn
        .batch_execute(&format!(
            "DROP SCHEMA IF EXISTS {} CASCADE; DROP SCHEMA IF EXISTS {} CASCADE;",
            q(&format!("{schema}_migrations")),
            q(&schema),
        ))
        .await;
    let _ = conn
        .batch_execute(&format!(
            "DO $$ BEGIN IF EXISTS (SELECT 1 FROM pg_roles WHERE rolname='{r}') THEN \
                EXECUTE 'REASSIGN OWNED BY {rq} TO current_user'; \
                EXECUTE 'DROP OWNED BY {rq}'; \
                EXECUTE 'DROP ROLE {rq}'; \
             END IF; END $$;",
            r = role.replace('\'', "''"),
            rq = q(&role),
        ))
        .await;
}

async fn table_exists(conn: &Client, app_id: &Uuid, table: &str) -> bool {
    let schema = app_id.to_string();
    let rows = conn
        .query(
            "SELECT 1 FROM information_schema.tables WHERE table_schema=$1 AND table_name=$2",
            &[&schema, &table],
        )
        .await
        .expect("information_schema query");
    !rows.is_empty()
}

/// Completed rows in the per-app journal, or 0 when the journal does not exist.
async fn journaled_count(conn: &Client, app_id: &Uuid) -> i64 {
    let meta = format!("{}_migrations", app_id);
    let q = format!("\"{}\".schema_migrations", meta.replace('"', "\"\""));
    let lit = q.replace('\'', "''");
    let present = conn
        .query(&format!("SELECT to_regclass('{lit}') IS NOT NULL AS p"), &[])
        .await
        .expect("regclass probe");
    if !present[0].get::<_, bool>("p") {
        return 0;
    }
    let rows = conn
        .query(
            &format!("SELECT count(*)::int8 AS n FROM {q} WHERE phase = 'completed'"),
            &[],
        )
        .await
        .expect("count journal");
    rows[0].get::<_, i64>("n")
}

/// SAFETY: the e2e runs single-threaded (`--test-threads=1`) and is the only
/// toucher of `ZEROSHIP_COLUMN_KEY_DEFAULT` in this binary.
#[allow(unsafe_code)]
fn set_column_key_env() {
    // 64 hex chars = 32 bytes.
    // SAFETY: the e2e binary runs single-threaded (`--test-threads=1`) and is the
    // only toucher of `ZEROSHIP_COLUMN_KEY_DEFAULT`; no concurrent env access.
    unsafe { std::env::set_var("ZEROSHIP_COLUMN_KEY_DEFAULT", "c".repeat(64)) };
}

// ===========================================================================
// THE CAPSTONE
// ===========================================================================

#[allow(unsafe_code)]
#[compio::test]
async fn schema_authority_capstone_end_to_end_on_real_pg() {
    let Some(conn) = admin_client().await else {
        return; // skip — DB unreachable
    };

    // A fresh app id ⇒ a fresh per-app schema "<app_id>" with no cross-test
    // contention (the engine's advisory lock + journal + role are per-app).
    let app_id = Uuid::now_v7();
    cleanup_app(&conn, &app_id).await;
    let schema = app_id.to_string();

    // The encryption key getter (`__zeroship_admin.get_column_key`) must exist
    // so the resolver returns NULL → env fallback rather than erroring.
    let pool = Rc::new(Pool::connect(&pg_url(), 4).await.expect("pool connect"));
    zeroship_plugin_db::auth::ensure_admin_schema(&pool)
        .await
        .expect("bootstrap __zeroship_admin");
    set_column_key_env();

    // T13 — the capstone's `docs.location` geoPoint needs PostGIS on the same
    // instance (alongside pgvector). If it cannot be installed (the standing
    // pgvector image may not bundle PostGIS), SKIP rather than fail: the geoPoint
    // declarative round-trip has dedicated coverage in
    // `zeroship-migrate/tests/declarative_pg.rs::t13_geopoint_*`.
    if conn
        .batch_execute("CREATE EXTENSION IF NOT EXISTS postgis")
        .await
        .is_err()
    {
        eprintln!("SKIP: PostGIS not available on the capstone instance (needs pgvector + postgis)");
        cleanup_app(&conn, &app_id).await;
        return;
    }

    // -------------------------------------------------------------------
    // SEAM 1 (P3): schema.js → IR → versioned migration file.
    // REAL fn: zeroship_migrate_js::generate_migration
    //   (eval in V8 → CollectionDescriptor[] → desired_snapshot →
    //    DeclarativeAuthor::diff → render dbmate file).
    // The migration is QUALIFIED into schema "<app_id>" (project_schema), so the
    // exact SQL the engine applies at deploy lands in the per-app schema.
    // -------------------------------------------------------------------
    let gen_dir = std::env::temp_dir().join(format!("p7-gen-{}", app_id.simple()));
    let owner_app = format!("app_{}", app_id.simple());
    let outcome = zeroship_migrate_js::generate_migration(
        SCHEMA_JS,
        &pg_url(),
        &schema,
        &owner_app,
        "initial_schema",
        &gen_dir,
    )
    .await
    .expect("P3 generate must succeed");

    let mig_path = outcome
        .written
        .expect("P3 generate writes a migration for a fresh schema");
    let body = std::fs::read_to_string(&mig_path).expect("read generated migration");
    let mig_name = mig_path
        .file_name()
        .unwrap()
        .to_string_lossy()
        .to_string();

    // --- Seam-1 assertions: every goodie survives schema → generate. ---
    assert!(body.contains("-- migrate:up") && body.contains("-- migrate:down"));
    assert!(
        body.contains(&format!("\"{schema}\".\"users\""))
            && body.contains(&format!("\"{schema}\".\"docs\"")),
        "both tables qualified into the per-app schema; body:\n{body}"
    );
    // vector(3) DDL.
    assert!(body.contains("vector(3)"), "vector column DDL; body:\n{body}");
    // T12 vector-ANN index: the engine emits a USING ivfflat index with the
    // cosine opclass (the data plane's flat-scan fallback is gone).
    assert!(
        body.contains("USING ivfflat") && body.contains("vector_cosine_ops"),
        "vector ANN index DDL (ivfflat + cosine opclass); body:\n{body}"
    );
    // T12 FTS: a `__fts` STORED generated tsvector column + a USING gin index,
    // emitted declaratively by the engine (no trigger, core PG).
    assert!(
        body.contains("\"__fts\" tsvector GENERATED ALWAYS AS (to_tsvector("),
        "FTS __fts generated tsvector column; body:\n{body}"
    );
    assert!(
        body.contains("USING gin (\"__fts\")"),
        "FTS GIN index over __fts; body:\n{body}"
    );
    // T13 geoPoint: a `geography(POINT, 4326)` column + a `docs_location_idx`
    // GiST index, emitted declaratively by the engine (mirroring plugin-db's
    // runtime `ensure_spatial_index`). Pre-T13 the engine modeled NO geo index.
    assert!(
        body.contains("geography(POINT, 4326)"),
        "geoPoint column DDL; body:\n{body}"
    );
    assert!(
        body.contains("USING gist (\"location\")"),
        "geoPoint GiST index over location; body:\n{body}"
    );
    // encrypted ssn → BYTEA + the inline /* zsenc:... */ sentinel (SQLite form)
    // AND a recoverable zsenc COMMENT (the PG form, since PG discards the inline
    // comment at parse time). This is the P4-HALF-A engine emission — see the
    // FINDING in the report: the stale comment in migrate-js/tests/generate_pg.rs
    // claims these are NOT emitted; this capstone proves they now ARE.
    assert!(
        body.to_lowercase().contains("\"ssn\" bytea"),
        "encrypted column is BYTEA; body:\n{body}"
    );
    assert!(
        body.contains("/* zsenc:"),
        "inline zsenc sentinel present on the encrypted column; body:\n{body}"
    );
    assert!(
        body.contains("zsenc:deterministic:default:string"),
        "zsenc sentinel body carries mode:keyId:wraps; body:\n{body}"
    );
    // masked siblings + the __zsmask COMMENT sentinel.
    assert!(
        body.contains("\"ssn_masked\" text") && body.contains("\"phone_masked\" text"),
        "encrypted + explicitly-masked fields get _masked siblings; body:\n{body}"
    );
    assert!(
        body.contains("__zsmask:kind=") && body.contains("classification="),
        "__zsmask sentinel COMMENT present; body:\n{body}"
    );
    // FK with ON DELETE CASCADE (docs.authorId → users).
    assert!(
        body.contains("REFERENCES") && body.to_uppercase().contains("ON DELETE CASCADE"),
        "FK clause with ON DELETE CASCADE; body:\n{body}"
    );

    // -------------------------------------------------------------------
    // SEAM 2 (P6 bundle): pack the migration into a .zship (manifest.migrations[]
    // + content-addressed blob), ingest into a blob store, read it back by hash,
    // reconstruct on disk — exactly what control's reconstruct_migration_files
    // does, but driving the REAL zeroship_bundle pack/ingest + blob store.
    // -------------------------------------------------------------------
    let blob_hash = zeroship_bundle::sha256_hex(body.as_bytes());
    let mut manifest = zeroship_bundle::Manifest::default();
    manifest.metadata.built_at = "2026-06-19T00:00:00Z".into();
    manifest.metadata.compiler = Some("p7-capstone".into());
    manifest.migrations = vec![zeroship_bundle::MigrationFileEntry {
        name: mig_name.clone(),
        hash: blob_hash.clone(),
    }];
    manifest.validate().expect("manifest with migrations validates");
    let manifest_json = serde_json::to_vec(&manifest).expect("serialize manifest");

    // Pack a real .zship: manifest.json FIRST, then blobs/<hash> (tar → zstd).
    let zship = build_zship(&manifest_json, &[(blob_hash.clone(), body.as_bytes().to_vec())]);

    // Ingest the .zship into a content-addressed blob store (the REAL fn).
    let store_dir = std::env::temp_dir().join(format!("p7-blobs-{}", app_id.simple()));
    let blob_store: std::sync::Arc<dyn zeroship_bundle::BlobStore> = std::sync::Arc::new(
        zeroship_bundle::LocalDiskBlobStore::new(store_dir.clone()).expect("blob store"),
    );
    let ingest = zeroship_bundle::ingest(&blob_store, &app_id, &zship)
        .await
        .expect("P6 ingest of the .zship");
    // The ingested manifest carries our migration entry.
    let ingested: zeroship_bundle::Manifest =
        serde_json::from_str(&ingest.manifest_json).expect("reparse ingested manifest");
    assert_eq!(ingested.migrations.len(), 1, "one migration carried");
    assert_eq!(ingested.migrations[0].name, mig_name);

    // Reconstruct the migration file from the blob store by hash (mirrors
    // control::api::reconstruct_migration_files).
    let deploy_dir = std::env::temp_dir().join(format!("p7-deploy-{}", app_id.simple()));
    std::fs::create_dir_all(&deploy_dir).expect("mkdir deploy dir");
    for entry in &ingested.migrations {
        let bytes = blob_store
            .get_blob(&entry.hash)
            .await
            .expect("blob present in store");
        std::fs::write(deploy_dir.join(&entry.name), bytes.as_ref())
            .expect("write reconstructed migration");
    }
    // Seam-2 assertion: the reconstructed bytes are byte-identical to generate's.
    let reconstructed =
        std::fs::read_to_string(deploy_dir.join(&mig_name)).expect("read reconstructed");
    assert_eq!(
        reconstructed, body,
        "the migration survives pack → ingest → blob → reconstruct byte-for-byte"
    );

    // -------------------------------------------------------------------
    // SEAM 3 (P6 deploy-apply): the REAL control deploy-migrate fn provisions
    // the per-app schema "<app_id>" + migrator_<app_id> role and applies the
    // bundle's migration under the Confined profile, BEFORE go-live.
    // REAL fn: zeroship_control::deploy_migrate::apply_bundle_migrations
    //
    // The engine renders the pgvector column type as UNQUALIFIED `vector(N)`, and
    // pgvector installs that type into `public`. The Confined migrator's
    // search_path is `"<app_id>", public` — the per-app schema is the sole
    // writable resolution target, `public` rides at the end for USAGE-only
    // resolution of the shared extension type (see `role::provision_migrator` +
    // `db.rs::search_path_clause`; matches plugin-db's runtime). So the
    // AS-GENERATED unqualified `vector(N)` migration applies cleanly under
    // deploy-apply — no qualify-rewrite, no workaround.
    //
    // (Previously this seam did NOT cohere: search_path pinned the per-app schema
    // only, so unqualified `vector` failed `type "vector" does not exist`. The
    // FIX 1 search_path widening — USAGE-only on `public` — closed that gap; the
    // confinement is unchanged: the migrator still cannot CREATE/write in
    // `public`, only resolve the extension type there.)
    let apply = zeroship_control::deploy_migrate::apply_bundle_migrations(
        &pg_url(),
        &app_id,
        &deploy_dir, // the as-generated (unqualified `vector(N)`) body
    )
    .await
    .expect(
        "P6 deploy-apply of the as-generated unqualified vector(N) migration must \
         succeed under the Confined migrator (public on search_path for USAGE-only \
         extension-type resolution)",
    );
    assert_eq!(apply.applied.len(), 1, "the bundle's migration applied once");
    assert!(apply.skipped.is_empty());

    // Seam-3 assertions: the engine created the goodie schema in "<app_id>".
    assert!(table_exists(&conn, &app_id, "users").await, "users table created");
    assert!(table_exists(&conn, &app_id, "docs").await, "docs table created");
    let journal_after_deploy = journaled_count(&conn, &app_id).await;
    assert_eq!(journal_after_deploy, 1, "the migration is journaled by the engine");

    // The engine's sentinels are live on the catalog (the contract the data plane
    // reads). Prove the zsenc COMMENT survived to the live column.
    let enc_comment = conn
        .query(
            "SELECT col_description(($1||'.users')::regclass, \
             (SELECT attnum FROM pg_attribute \
              WHERE attrelid = ($1||'.users')::regclass AND attname = 'ssn')) AS c",
            &[&format!("\"{schema}\"")],
        )
        .await
        .expect("col_description for ssn");
    let ssn_comment: Option<String> = enc_comment[0].get("c");
    assert!(
        ssn_comment.as_deref().unwrap_or("").contains("zsenc:deterministic:default"),
        "engine wrote the recoverable zsenc COMMENT on the live ssn column, got {ssn_comment:?}"
    );

    // -------------------------------------------------------------------
    // SEAM 4 (P4/P5): plugin-db data plane on the ENGINE-created schema.
    //   (a) the PG registerModel dispatch issues NO DDL (the cutover) — the
    //       journal count must be UNCHANGED after it runs (sole-applier proof);
    //   (b) runtime_schema_for introspects the live catalog + sentinels and
    //       recovers the encrypted + masked metadata (no declared schema);
    //   (c) the write pipeline AEAD-encrypts ssn + derives the masked sibling;
    //   (d) the read pipeline decrypts ssn to plaintext + wraps the masked col;
    //   (e) the vector column accepts a vector value.
    // REAL fns: register_model::exec_register_model_via_dispatch_for_tests,
    //           crud::{runtime_schema_for_tests, prepare_insert_many_docs_for_write,
    //           finalize_rows_on_read_for_tests}.
    // -------------------------------------------------------------------
    zeroship_plugin_db::set_postgres_pool_for_tests(Rc::clone(&pool), &pg_url());

    // (a) The PG production dispatch must NOT migrate (engine is the sole PG
    //     authority). The declared schema here is what the SDK would emit; on PG
    //     the dispatch no-ops the apply.
    let declared_users = serde_json::json!({
        "email": {"type": "string", "required": true},
        "ssn": {"type": "string",
                "encrypted": {"mode": "deterministic", "keyId": "default", "wraps": "string"}},
        "phone": {"type": "string", "mask": {"kind": "last4", "classification": "pci"}},
        "age": {"type": "number"},
    });
    zeroship_plugin_db::register_model::exec_register_model_via_dispatch_for_tests(
        &schema,
        "users",
        &declared_users,
        &serde_json::json!([]),
    )
    .await
    .expect("PG registerModel dispatch must succeed (no-op apply)");
    assert_eq!(
        journaled_count(&conn, &app_id).await,
        journal_after_deploy,
        "SOLE-APPLIER PROOF: the runtime registerModel added ZERO journal rows — \
         the engine remains the only schema authority"
    );

    // Readiness gate (the dispatch caller marks the model in prod; the
    // via-dispatch seam stops at exec_register_model, so mirror it).
    zeroship_plugin_db::mark_model_registered_for_tests(&schema, "users");

    // (b) introspection recovers the goodies from the live catalog + sentinels.
    let introspected = zeroship_plugin_db::crud::runtime_schema_for_tests(&schema, "users")
        .await
        .expect("introspect users")
        .expect("users has goodies");
    assert_eq!(
        introspected["ssn"]["encrypted"]["mode"], "deterministic",
        "encrypted meta recovered from the live zsenc sentinel"
    );
    assert_eq!(
        introspected["phone"]["mask"]["kind"], "last4",
        "mask meta recovered from the live __zsmask sentinel"
    );

    // (c) WRITE through the real pipeline (introspected metadata).
    let mut docs = serde_json::json!([{
        "id": "usr_p7_ada",
        "email": "ada@example.com",
        "ssn": "123-45-6789",
        "phone": "415-555-0142",
        "age": 36,
    }]);
    zeroship_plugin_db::crud::prepare_insert_many_docs_for_write(&mut docs, &schema, "users", None)
        .await
        .expect("write pipeline");
    let doc = &docs[0];
    assert!(
        doc["ssn"].as_str().is_some() && doc["ssn"] != serde_json::json!("123-45-6789"),
        "ssn replaced by ciphertext on write, got {:?}",
        doc["ssn"]
    );
    assert_eq!(doc["__zsenc__ssn"], serde_json::json!(true), "encrypt marker set");
    assert_eq!(
        doc["phone_masked"], serde_json::json!("***-***-0142"),
        "mask pass derives the last4 sibling, got {:?}",
        doc["phone_masked"]
    );

    // Persist the way the SQL builder would (decode ciphertext, store sibling).
    let ssn_b64 = doc["ssn"].as_str().unwrap().to_string();
    let phone_masked = doc["phone_masked"].as_str().unwrap().to_string();
    // The engine-generated table carries the platform system fields as NOT NULL
    // (created_at/updated_at/version) — in production the data plane's
    // system_fields_pass stamps them; this raw INSERT supplies them directly.
    pool.execute(
        &format!(
            "INSERT INTO \"{schema}\".\"users\" \
             (id, email, ssn, phone, phone_masked, age, created_at, updated_at, version) \
             VALUES ($1, $2, decode($3, 'base64')::bytea, $4, $5, $6, now(), now(), 1)"
        ),
        &[
            &"usr_p7_ada",
            &"ada@example.com",
            &ssn_b64.as_str(),
            &"415-555-0142",
            &phone_masked.as_str(),
            &36f64,
        ],
    )
    .await
    .expect("insert users row");

    // (e) the vector column accepts a vector AND (T13) the geography column
    //     accepts a real point — insert a docs row (FK → users).
    pool.execute(
        &format!(
            "INSERT INTO \"{schema}\".\"docs\" \
             (id, title, embedding, location, \"authorId\", created_at, updated_at, version) \
             VALUES ($1, $2, '[0.1,0.2,0.3]'::vector, \
                     ST_MakePoint($4, $5)::geography, $3, now(), now(), 1)"
        ),
        &[
            &"doc_p7_1",
            &"Notes on Babbage",
            &"usr_p7_ada",
            &(-122.4194f64),
            &37.7749f64,
        ],
    )
    .await
    .expect("insert docs row with a vector + geography point + FK to users");
    // The FK is real: an orphan authorId is rejected.
    let orphan = pool
        .execute(
            &format!(
                "INSERT INTO \"{schema}\".\"docs\" \
                 (id, title, embedding, location, \"authorId\", created_at, updated_at, version) \
                 VALUES ($1, $2, '[0.4,0.5,0.6]'::vector, \
                         ST_MakePoint(0, 0)::geography, $3, now(), now(), 1)"
            ),
            &[&"doc_p7_orphan", &"Orphan", &"usr_nobody"],
        )
        .await;
    assert!(orphan.is_err(), "FK must reject an orphan authorId");

    // (d) READ through the real pipeline (introspected metadata).
    let raw = pool
        .query_text_params(
            &format!(
                "SELECT id, email, encode(ssn, 'base64') AS ssn, \
                 phone_masked AS phone FROM \"{schema}\".\"users\" WHERE id = $1"
            ),
            &[&"usr_p7_ada"],
        )
        .await
        .expect("read raw users row");
    assert_eq!(raw.len(), 1);
    let row = serde_json::json!({
        "id": "usr_p7_ada",
        "email": "ada@example.com",
        "ssn": raw[0].get::<_, String>("ssn"),
        "phone": raw[0].get::<_, String>("phone"),
    });
    let finalized =
        zeroship_plugin_db::crud::finalize_rows_on_read_for_tests(&schema, "users", vec![row])
            .await
            .expect("read pipeline");
    let out = &finalized[0];

    // The explicitly-masked `phone` wraps into the platform MaskedValue sentinel,
    // carrying the last4 form + the introspected classification (pci).
    assert_eq!(out["phone"]["sentinel"], serde_json::json!("__zsmask__"), "phone wrapped");
    assert_eq!(out["phone"]["masked"], serde_json::json!("***-***-0142"));
    assert_eq!(out["phone"]["classification"], serde_json::json!("pci"));

    // FINDING (privacy-correct, but worth recording): `ssn` is `t.encrypted`,
    // which AUTO-MASKS as PII — the engine's `generate` emitted BOTH a `zsenc`
    // COMMENT on `ssn` AND a `__zsmask` on the `ssn_masked` sibling, and
    // introspection recovers `ssn` as a masked column. So a DEFAULT read
    // (`ApplyOptions::default()` — what `finalize_rows_on_read_for_tests` uses,
    // wrap_masked=true, no `unmask_columns`) surfaces `ssn` as a MASKED sentinel,
    // NOT the decrypted plaintext. This is the privacy-preserving default for an
    // encrypted-and-auto-masked column; plaintext requires an explicit
    // unmask/decrypt-scoped read (the `unmask_columns` option, which the test
    // helper does not expose). We assert the masked surface here…
    assert_eq!(
        out["ssn"]["sentinel"], serde_json::json!("__zsmask__"),
        "encrypted+auto-masked ssn surfaces masked on a default read, got {:?}",
        out["ssn"]
    );

    // …and prove the stored bytes ARE recoverable plaintext by decrypting them
    // through the REAL encryption machinery with the same key + deterministic AAD
    // the write pipeline used — the byte-correct end-to-end encryption proof.
    {
        use base64::Engine as _;
        use zeroship_plugin_db::backend::{EncryptedColumn as _, EncryptionMode, PostgresBackend};

        let backend = PostgresBackend::new(Rc::clone(&pool), pg_url());
        // Key derivation uses the per-app id = the schema (matches the write).
        let key = backend
            .resolve_key(&schema, "default")
            .await
            .expect("resolve column key");
        // Deterministic mode omits row_pk from the AAD (see encryption::aad).
        let aad = zeroship_plugin_db::encryption::canonical_aad("users", "ssn", None);
        let ct_b64: String = raw[0].get("ssn");
        let ct = base64::engine::general_purpose::STANDARD
            .decode(ct_b64.as_bytes())
            .expect("base64-decode the stored ciphertext");
        let recovered = backend
            .decrypt(&key, EncryptionMode::Deterministic, &ct, &aad)
            .expect("decrypt the stored ssn ciphertext");
        assert_eq!(
            recovered, b"123-45-6789",
            "the encrypted ssn round-trips to plaintext via the real AEAD path"
        );
    }

    // Read the vector back (byte-correct round-trip on the vector column).
    let vrow = pool
        .query_text_params(
            &format!("SELECT embedding::text AS e FROM \"{schema}\".\"docs\" WHERE id = $1"),
            &[&"doc_p7_1"],
        )
        .await
        .expect("read vector");
    assert_eq!(
        vrow[0].get::<_, String>("e"),
        "[0.1,0.2,0.3]",
        "the vector column round-trips the inserted vector"
    );

    // T13 — the geography column is functional: a `ST_DWithin` spatial query
    // (the operator plugin-db's distance search uses) finds the inserted point
    // within 5km of a nearby origin, proving the engine-emitted
    // `geography(POINT, 4326)` column + GiST index cohere end-to-end.
    let near = pool
        .query_text_params(
            &format!(
                "SELECT id FROM \"{schema}\".\"docs\" \
                 WHERE ST_DWithin(location, ST_MakePoint($1, $2)::geography, $3)"
            ),
            &[&"-122.42", &"37.77", &"5000"],
        )
        .await
        .expect("ST_DWithin spatial query on the geography column");
    assert_eq!(near.len(), 1, "the geoPoint is within 5km of the query origin");
    assert_eq!(near[0].get::<_, String>("id"), "doc_p7_1");

    // FINAL sole-applier proof: after the full CRUD round-trip, the engine's
    // journal is STILL exactly what the deploy-apply left — the data plane
    // applied no schema change of its own.
    assert_eq!(
        journaled_count(&conn, &app_id).await,
        journal_after_deploy,
        "CRUD must not have triggered any runtime DDL"
    );

    // ---- cleanup (hygiene) ----
    let _ = std::fs::remove_dir_all(&gen_dir);
    let _ = std::fs::remove_dir_all(&deploy_dir);
    let _ = std::fs::remove_dir_all(&store_dir);
    cleanup_app(&conn, &app_id).await;
}

/// Pack `(name, bytes)` blob entries + a manifest into a tar archive, then
/// zstd-compress — the `.zship` wire format. manifest.json goes FIRST (the
/// ingest contract requires it). Mirrors control's `build_zship` test helper.
fn build_zship(manifest_bytes: &[u8], blobs: &[(String, Vec<u8>)]) -> Vec<u8> {
    use std::io::Write as _;
    let mut tar_buf: Vec<u8> = Vec::new();
    {
        let mut builder = tar::Builder::new(&mut tar_buf);
        let mut header = tar::Header::new_gnu();
        header.set_size(manifest_bytes.len() as u64);
        header.set_mode(0o644);
        header.set_cksum();
        builder
            .append_data(&mut header, "manifest.json", manifest_bytes)
            .expect("append manifest");
        for (hash, bytes) in blobs {
            let mut h = tar::Header::new_gnu();
            h.set_size(bytes.len() as u64);
            h.set_mode(0o644);
            h.set_cksum();
            builder
                .append_data(&mut h, format!("blobs/{hash}"), bytes.as_slice())
                .expect("append blob");
        }
        builder.finish().expect("tar finish");
    }
    let mut compressed: Vec<u8> = Vec::new();
    {
        let mut enc = zstd::Encoder::new(&mut compressed, 0).expect("zstd enc");
        enc.write_all(&tar_buf).expect("zstd write");
        enc.finish().expect("zstd finish");
    }
    compressed
}

// A path-marker so a future cfg-out never trips an unused-import lint.
#[allow(dead_code)]
fn _path_marker(_: &Path, _: &PathBuf) {}
