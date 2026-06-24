//! Faithful integration tests for the P6 deploy-migrate step
//! (`zeroship_control::deploy_migrate::apply_bundle_migrations`).
//!
//! These drive the REAL apply path — `zeroship-migrate`'s `load_dir` +
//! `engine.plan(Confined)` + `engine.apply` against a real Postgres — exactly
//! as the control deploy handler does, NOT a shim. The handler's only extra
//! layer (reconstructing the migration files from blobs into a tmp dir) is
//! itself a thin file write; the load-bearing logic (provision schema + role,
//! apply under Confined, refuse-destructive, go-live gating) is what these
//! exercise directly with a hand-authored migration directory.
//!
//! Requires `zeroship_migrate_test` on :5440 (psql password `zeroship`). When
//! the DB is unreachable the tests are skipped silently (matching the
//! `CONTROL_TEST_DB`/`PG_TEST_URL` pattern in `deploy_test.rs`).

use std::path::{Path, PathBuf};

use compio_postgres::NoTls;
use uuid::Uuid;

use zeroship_control::deploy_migrate::{
    apply_bundle_migrations, apply_bundle_migrations_approved, DeployMigrateError,
};

/// Admin DSN with CREATEROLE + CREATE SCHEMA (the `postgres` superuser), the
/// same DB the migrate crate's own integration tests use.
const ADMIN_DSN: &str =
    "host=localhost port=5440 user=postgres password=zeroship dbname=zeroship_migrate_test";

fn admin_dsn() -> String {
    std::env::var("MIGRATE_TEST_DB").unwrap_or_else(|_| ADMIN_DSN.to_string())
}

/// Open a raw admin connection for the assert side, or `None` when the DB is
/// unreachable (⇒ the test skips).
///
/// **CI hard-gate (F-LOW, code-critic).** When `MIGRATE_REQUIRE_DB` is set
/// (CI MUST set it), an unreachable `:5440` is a HARD test FAILURE, not a silent
/// green skip. Otherwise a misconfigured CI Postgres would let the whole
/// faithful-deploy security suite (the bare-name `dropIndex` refusal, the
/// understated-unique refusal, the cross-tenant ownership refusal) vacuously
/// PASS — masking a real regression. Locally (`MIGRATE_REQUIRE_DB` unset) the
/// skip is retained so a dev without the test DB can still run the rest of the
/// workspace.
async fn admin_conn() -> Option<compio_postgres::Client> {
    match compio_postgres::connect(&admin_dsn(), NoTls).await {
        Ok((client, conn)) => {
            compio::runtime::spawn(async move {
                let _ = conn.run().await;
            })
            .detach();
            Some(client)
        }
        Err(e) => {
            assert!(
                std::env::var("MIGRATE_REQUIRE_DB").is_err(),
                "MIGRATE_REQUIRE_DB is set but zeroship_migrate_test on :5440 is unreachable \
                 ({e}); the faithful-deploy security suite must NOT silently skip in CI — \
                 a missing test DB is a hard failure, not a vacuous green pass"
            );
            None
        }
    }
}

/// A fresh per-test app id ⇒ a fresh schema `"<app_id>"` (no cross-test
/// contention; the engine's advisory lock + journal are per-app).
fn fresh_app_id() -> Uuid {
    Uuid::now_v7()
}

/// Write a set of `(filename, body)` migration files into a fresh tmp dir,
/// returning the dir path (the caller removes it).
fn migrations_dir(files: &[(&str, &str)]) -> PathBuf {
    let mut dir = std::env::temp_dir();
    dir.push(format!("zship-mig-test-{}", Uuid::new_v4().simple()));
    std::fs::create_dir_all(&dir).expect("mkdir tmp migrations");
    for (name, body) in files {
        std::fs::write(dir.join(name), body).expect("write migration file");
    }
    dir
}

/// Drop a per-app schema + its meta schema + migrator role so a re-run is clean.
async fn cleanup_app(conn: &compio_postgres::Client, app_id: &Uuid) {
    let schema = app_id.to_string();
    let role = zeroship_migrate::migrator_role_name(&schema).unwrap();
    // The migrator owns the schema; reassign before drop so the cascade works.
    let q = |s: &str| format!("\"{}\"", s.replace('"', "\"\""));
    let _ = conn
        .batch_execute(&format!(
            "DROP SCHEMA IF EXISTS {} CASCADE; \
             DROP SCHEMA IF EXISTS {} CASCADE;",
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

/// Does table `table_name` exist in schema `"<app_id>"`?
async fn table_exists(conn: &compio_postgres::Client, app_id: &Uuid, table: &str) -> bool {
    let schema = app_id.to_string();
    let rows = conn
        .query(
            "SELECT 1 FROM information_schema.tables \
             WHERE table_schema = $1 AND table_name = $2",
            &[&schema, &table],
        )
        .await
        .expect("query information_schema");
    !rows.is_empty()
}

/// How many migrations are journaled completed for this app? Returns 0 when
/// the per-app meta schema / journal table does not exist yet (a refused plan
/// never bootstraps the journal).
async fn journaled_count(conn: &compio_postgres::Client, app_id: &Uuid) -> i64 {
    let meta = format!("{}_migrations", app_id);
    let q = format!("\"{}\".schema_migrations", meta.replace('"', "\"\""));
    let lit = q.replace('\'', "''");
    // Probe existence first (a refused plan never bootstraps the journal); a
    // direct `FROM <missing table>` would parse-error even inside a CASE arm.
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

// ---------------------------------------------------------------------------
// Happy path: a CREATE TABLE migration is applied + journaled.
// ---------------------------------------------------------------------------

#[compio::test]
async fn deploy_migrate_applies_create_table_and_journals() {
    let Some(conn) = admin_conn().await else {
        eprintln!("SKIP: zeroship_migrate_test :5440 unreachable");
        return;
    };
    let app_id = fresh_app_id();
    cleanup_app(&conn, &app_id).await;

    let dir = migrations_dir(&[(
        "V0001__create_widgets.sql",
        "CREATE TABLE widgets (id bigint PRIMARY KEY, name text NOT NULL);",
    )]);

    let outcome = apply_bundle_migrations(&admin_dsn(), &app_id, &dir)
        .await
        .expect("deploy-migrate must succeed");
    assert_eq!(outcome.applied.len(), 1, "one migration applied");
    assert!(outcome.skipped.is_empty(), "nothing skipped on first apply");

    // The table exists in the per-app schema "<app_id>".
    assert!(
        table_exists(&conn, &app_id, "widgets").await,
        "widgets table must exist in schema {app_id}"
    );
    // The migration is journaled in the per-app meta schema.
    assert_eq!(journaled_count(&conn, &app_id).await, 1, "migration journaled");

    let _ = std::fs::remove_dir_all(&dir);
    cleanup_app(&conn, &app_id).await;
}

// ---------------------------------------------------------------------------
// Idempotency: a re-deploy of the same migrations is a no-op (already
// journaled), proving the deploy path is roll-forward safe.
// ---------------------------------------------------------------------------

#[compio::test]
async fn deploy_migrate_is_idempotent_across_redeploys() {
    let Some(conn) = admin_conn().await else {
        eprintln!("SKIP: zeroship_migrate_test :5440 unreachable");
        return;
    };
    let app_id = fresh_app_id();
    cleanup_app(&conn, &app_id).await;

    let dir = migrations_dir(&[(
        "V0001__create_gadgets.sql",
        "CREATE TABLE gadgets (id bigint PRIMARY KEY);",
    )]);

    let first = apply_bundle_migrations(&admin_dsn(), &app_id, &dir)
        .await
        .expect("first apply");
    assert_eq!(first.applied.len(), 1);

    // Second deploy of the SAME set: nothing new applied (all journaled).
    let second = apply_bundle_migrations(&admin_dsn(), &app_id, &dir)
        .await
        .expect("second apply (idempotent)");
    assert!(
        second.applied.is_empty(),
        "re-deploy applies nothing new, got {:?}",
        second.applied
    );
    assert_eq!(journaled_count(&conn, &app_id).await, 1, "still one journaled");

    let _ = std::fs::remove_dir_all(&dir);
    cleanup_app(&conn, &app_id).await;
}

// ---------------------------------------------------------------------------
// Failure: a destructive migration is REFUSED at deploy (Approval::None) and
// the table is NOT created — the go-live gate (the caller's "on error, no
// commit") therefore holds.
// ---------------------------------------------------------------------------

#[compio::test]
async fn deploy_migrate_refuses_destructive_and_creates_nothing() {
    let Some(conn) = admin_conn().await else {
        eprintln!("SKIP: zeroship_migrate_test :5440 unreachable");
        return;
    };
    let app_id = fresh_app_id();
    cleanup_app(&conn, &app_id).await;

    // A single migration that DROPs a table is destructive ⇒ refused at deploy.
    let dir = migrations_dir(&[(
        "V0001__drop_legacy.sql",
        "DROP TABLE IF EXISTS legacy;",
    )]);

    let err = apply_bundle_migrations(&admin_dsn(), &app_id, &dir)
        .await
        .expect_err("destructive migration must be refused at deploy");
    match err {
        DeployMigrateError::Apply(_) => {}
        other => panic!("expected Apply(ApprovalRequired), got {other:?}"),
    }

    // Nothing was applied: the journal has no completed rows (the table the
    // migration referenced was never created either).
    assert_eq!(
        journaled_count(&conn, &app_id).await,
        0,
        "a refused destructive plan journals nothing"
    );

    let _ = std::fs::remove_dir_all(&dir);
    cleanup_app(&conn, &app_id).await;
}

// ---------------------------------------------------------------------------
// Failure: a malformed migration filename is a load error (no DB touched).
// ---------------------------------------------------------------------------

#[compio::test]
async fn deploy_migrate_rejects_bad_migration_filename() {
    let Some(conn) = admin_conn().await else {
        eprintln!("SKIP: zeroship_migrate_test :5440 unreachable");
        return;
    };
    let app_id = fresh_app_id();
    cleanup_app(&conn, &app_id).await;

    // `garbage.sql` matches no migration grammar ⇒ a hard LoaderError.
    let dir = migrations_dir(&[("garbage.sql", "CREATE TABLE t (id int);")]);

    let err = apply_bundle_migrations(&admin_dsn(), &app_id, &dir)
        .await
        .expect_err("unrecognized filename must fail the load");
    assert!(
        matches!(err, DeployMigrateError::Load(_)),
        "expected Load error, got {err:?}"
    );

    let _ = std::fs::remove_dir_all(&dir);
    cleanup_app(&conn, &app_id).await;
}

// ---------------------------------------------------------------------------
// Two-migration set applies in order; the second references the first's table.
// ---------------------------------------------------------------------------

#[compio::test]
async fn deploy_migrate_applies_multiple_in_order() {
    let Some(conn) = admin_conn().await else {
        eprintln!("SKIP: zeroship_migrate_test :5440 unreachable");
        return;
    };
    let app_id = fresh_app_id();
    cleanup_app(&conn, &app_id).await;

    let dir = migrations_dir(&[
        (
            "V0001__create_orders.sql",
            "CREATE TABLE orders (id bigint PRIMARY KEY);",
        ),
        (
            "V0002__add_orders_total.sql",
            "ALTER TABLE orders ADD COLUMN total numeric NOT NULL DEFAULT 0;",
        ),
    ]);

    let outcome = apply_bundle_migrations(&admin_dsn(), &app_id, &dir)
        .await
        .expect("apply two migrations");
    assert_eq!(outcome.applied.len(), 2, "both migrations applied");
    assert_eq!(journaled_count(&conn, &app_id).await, 2);

    // The ALTER (V0002) succeeded only because V0001 ran first — prove the
    // column exists.
    let schema = app_id.to_string();
    let rows = conn
        .query(
            "SELECT 1 FROM information_schema.columns \
             WHERE table_schema=$1 AND table_name='orders' AND column_name='total'",
            &[&schema],
        )
        .await
        .expect("columns query");
    assert!(!rows.is_empty(), "V0002's column must exist (ordering held)");

    let _ = std::fs::remove_dir_all(&dir);
    cleanup_app(&conn, &app_id).await;
}

// H2 — the deploy-migrate path now routes through the integrity-manifest seam
// (`apply_verified`). No trusted build-side stamp exists yet, so the deploy
// passes `expected: None` (Case 2 — traceability, not yet tamper-prevention). This
// test pins TWO things:
//   1. The deploy still applies correctly through the verified seam (no regression
//      vs the old direct `apply`).
//   2. The manifest the deploy computes over the LOADED set (the value it logs, and
//      the value a future trusted stamp will be compared against) is
//      tamper-SENSITIVE: editing a migration file's body yields a DIFFERENT
//      manifest. This is the foundation the H2 follow-up needs — once the build side
//      stamps + persists this hash out-of-band and the deploy passes `Some(&hash)`,
//      a tampered/reordered set is REFUSED before any DDL (the gate itself is
//      already proven in zeroship-migrate's manifest_pg.rs tamper/reorder tests).
#[compio::test]
async fn h2_deploy_routes_through_verified_seam_and_manifest_is_tamper_sensitive() {
    let Some(conn) = admin_conn().await else {
        eprintln!("SKIP: zeroship_migrate_test :5440 unreachable");
        return;
    };
    let app_id = fresh_app_id();
    cleanup_app(&conn, &app_id).await;

    let files: &[(&str, &str)] = &[
        (
            "V0001__create_widgets.sql",
            "CREATE TABLE widgets (id bigint PRIMARY KEY);",
        ),
        (
            "V0002__add_widgets_name.sql",
            "ALTER TABLE widgets ADD COLUMN name text;",
        ),
    ];
    let dir = migrations_dir(files);

    // (1) The deploy applies correctly through the verified seam.
    let outcome = apply_bundle_migrations(&admin_dsn(), &app_id, &dir)
        .await
        .expect("deploy-migrate via the verified seam must succeed");
    assert_eq!(outcome.applied.len(), 2, "both migrations applied");
    assert_eq!(journaled_count(&conn, &app_id).await, 2);

    // (2) The manifest over the loaded set is deterministic + tamper-sensitive: the
    //     same files load to the SAME manifest, but an edited body changes it. We
    //     load via the real `load_dir` (the same loader the deploy uses) so this is
    //     faithful to the value the deploy computes + logs.
    let set = zeroship_migrate::load_dir_migrations(&dir).expect("load original set");
    let manifest_a = zeroship_migrate::compute_manifest(&set);
    let manifest_a2 = zeroship_migrate::compute_manifest(&set);
    assert_eq!(
        manifest_a, manifest_a2,
        "the manifest must be deterministic over the same set"
    );

    // Tamper: edit V0002's body in a fresh dir, reload, recompute.
    let tampered_files: &[(&str, &str)] = &[
        files[0],
        (
            "V0002__add_widgets_name.sql",
            // A DIFFERENT body (an extra column) — the content the manifest folds.
            "ALTER TABLE widgets ADD COLUMN name text; ALTER TABLE widgets ADD COLUMN pwned text;",
        ),
    ];
    let tampered_dir = migrations_dir(tampered_files);
    let tampered_set = zeroship_migrate::load_dir_migrations(&tampered_dir).expect("load tampered set");
    let manifest_b = zeroship_migrate::compute_manifest(&tampered_set);
    assert_ne!(
        manifest_a, manifest_b,
        "editing a migration body MUST change the manifest (tamper-sensitive); this is \
         the property the H2 follow-up's trusted stamp will rely on to refuse a tampered set"
    );

    let _ = std::fs::remove_dir_all(&dir);
    let _ = std::fs::remove_dir_all(&tampered_dir);
    cleanup_app(&conn, &app_id).await;
}

// ===========================================================================
// CREATOR `.ir.json` PATH — driven through the REAL deploy entrypoint
// (`apply_bundle_migrations`), the production caller of the fail-closed IR LOAD
// GATE (`IrAuthor::load_and_lower`: ir_version → validate_ir → ownership →
// checksum → lower) + the per-dialect lower. NOT a unit test of the gate; the
// `.ir.json` is discovered in the migrations dir + gated + lowered + applied by
// the same code the control deploy handler runs.
// ===========================================================================

// Happy path: a valid creator `.ir.json` createTable is gated, lowered (PG), and
// APPLIED — the table exists + the migration is journaled.
#[compio::test]
async fn deploy_migrate_applies_valid_ir_json() {
    let Some(conn) = admin_conn().await else {
        eprintln!("SKIP: zeroship_migrate_test :5440 unreachable");
        return;
    };
    let app_id = fresh_app_id();
    cleanup_app(&conn, &app_id).await;

    // A `.ir.json` whose declarer is the deploying app (the gate's owner_app is
    // server-stamped from app_id by the deploy path, so we pass owner_app="" — the
    // gate stamps it). A fresh createTable is owned by the deployer (auto).
    let ir = r#"{"ir_version":1,"name":"create_notes","ops":[
        {"op":"createTable","name":"notes","columns":[
            {"name":"title","type":"text","nullable":false},
            {"name":"body","type":"text"}
        ]}
    ]}"#;
    let dir = migrations_dir(&[("0001_create_notes.ir.json", ir)]);

    let outcome = apply_bundle_migrations(&admin_dsn(), &app_id, &dir)
        .await
        .expect("a valid .ir.json must lower + apply on the real deploy path");
    assert!(
        !outcome.applied.is_empty(),
        "the lowered IR migration(s) must apply, got {:?}",
        outcome.applied
    );
    assert!(
        table_exists(&conn, &app_id, "notes").await,
        "the IR-created 'notes' table must exist in schema {app_id}"
    );
    assert!(
        journaled_count(&conn, &app_id).await >= 1,
        "the IR migration must be journaled"
    );

    let _ = std::fs::remove_dir_all(&dir);
    cleanup_app(&conn, &app_id).await;
}

/// Does column `col` exist on `<app_id>.<table>`?
async fn column_exists(
    conn: &compio_postgres::Client,
    app_id: &Uuid,
    table: &str,
    col: &str,
) -> bool {
    let schema = app_id.to_string();
    let rows = conn
        .query(
            "SELECT 1 FROM information_schema.columns \
             WHERE table_schema = $1 AND table_name = $2 AND column_name = $3",
            &[&schema, &table, &col],
        )
        .await
        .expect("query information_schema.columns");
    !rows.is_empty()
}

/// Does an index named `idx` exist in the per-app schema `<app_id>`?
async fn index_exists(conn: &compio_postgres::Client, app_id: &Uuid, idx: &str) -> bool {
    let schema = app_id.to_string();
    let rows = conn
        .query(
            "SELECT 1 FROM pg_indexes WHERE schemaname = $1 AND indexname = $2",
            &[&schema, &idx],
        )
        .await
        .expect("query pg_indexes");
    !rows.is_empty()
}

/// Does a FOREIGN KEY constraint exist on `<app_id>.<table>`?
async fn fk_exists(conn: &compio_postgres::Client, app_id: &Uuid, table: &str) -> bool {
    let schema = app_id.to_string();
    let rows = conn
        .query(
            "SELECT 1 FROM information_schema.table_constraints \
             WHERE table_schema = $1 AND table_name = $2 AND constraint_type = 'FOREIGN KEY'",
            &[&schema, &table],
        )
        .await
        .expect("query information_schema.table_constraints");
    !rows.is_empty()
}

// CROSS-FILE (code-critic HIGH): a MULTI-file `.ir.json` deploy where 0001
// createTable `notes` and 0002 addColumn + addConstraint(FK) on `notes`. The
// ownership registry + FK-inline live-set MUST advance as each file applies, so
// 0002 sees `notes` as owned-by-the-deployer + live. Pre-fix, the registry/
// live-set were introspected ONCE before the loop and never advanced, so 0002
// resolved `notes`→<unregistered> and FAILED CLOSED on ownership — a legitimate
// same-deploy migration. The 5 prior IR e2e are all SINGLE-FILE, so this case
// was wholly untested.
#[compio::test]
async fn deploy_migrate_ir_cross_file_addcolumn_fk_on_earlier_file_table() {
    let Some(conn) = admin_conn().await else {
        eprintln!("SKIP: zeroship_migrate_test :5440 unreachable");
        return;
    };
    let app_id = fresh_app_id();
    cleanup_app(&conn, &app_id).await;

    // 0001 creates `notes` and `tags`. 0002 (a SEPARATE file) adds a column on
    // `notes` AND a FK from `notes.tag_id` → `tags` — both touching tables created
    // in the PRIOR file. The FK must INLINE/defer against the now-live `tags`.
    let ir_0001 = r#"{"ir_version":1,"name":"create_notes","ops":[
        {"op":"createTable","name":"notes","columns":[
            {"name":"title","type":"text","nullable":false}
        ]},
        {"op":"createTable","name":"tags","columns":[
            {"name":"label","type":"text"}
        ]}
    ]}"#;
    let ir_0002 = r#"{"ir_version":1,"name":"link_notes_tags","ops":[
        {"op":"addColumn","table":"notes","column":"tag_id","type":{"ref":{"references":"tags"}}},
        {"op":"addConstraint","table":"notes","constraint":{
            "kind":{"kind":"fk","columns":["tag_id"],"referencesTable":"tags","referencesColumns":["id"]}
        }}
    ]}"#;
    let dir = migrations_dir(&[
        ("0001_create_notes.ir.json", ir_0001),
        ("0002_link_notes_tags.ir.json", ir_0002),
    ]);

    apply_bundle_migrations(&admin_dsn(), &app_id, &dir)
        .await
        .expect(
            "a 2-file IR deploy that touches an earlier file's table must apply \
             (the registry/live-set must advance across files)",
        );

    assert!(table_exists(&conn, &app_id, "notes").await, "0001 'notes' must exist");
    assert!(table_exists(&conn, &app_id, "tags").await, "0001 'tags' must exist");
    assert!(
        column_exists(&conn, &app_id, "notes", "tag_id").await,
        "0002 must add 'tag_id' to the 0001-created 'notes' table"
    );
    assert!(
        fk_exists(&conn, &app_id, "notes").await,
        "0002 must add the FK on 'notes' referencing the 0001-created 'tags'"
    );

    let _ = std::fs::remove_dir_all(&dir);
    cleanup_app(&conn, &app_id).await;
}

// A `.sql` and a `.ir.json` ship together: BOTH apply (the Flyway loader skips
// the IR file; the IR seam handles it). Proves the discovery branch coexists with
// the platform `.sql` path.
#[compio::test]
async fn deploy_migrate_applies_sql_and_ir_together() {
    let Some(conn) = admin_conn().await else {
        eprintln!("SKIP: zeroship_migrate_test :5440 unreachable");
        return;
    };
    let app_id = fresh_app_id();
    cleanup_app(&conn, &app_id).await;

    let sql = "CREATE TABLE legacy (id bigint PRIMARY KEY);";
    let ir = r#"{"ir_version":1,"name":"create_modern","ops":[
        {"op":"createTable","name":"modern","columns":[{"name":"label","type":"text"}]}
    ]}"#;
    let dir = migrations_dir(&[
        ("V0001__create_legacy.sql", sql),
        ("0002_create_modern.ir.json", ir),
    ]);

    let outcome = apply_bundle_migrations(&admin_dsn(), &app_id, &dir)
        .await
        .expect("both the .sql and .ir.json must apply");
    assert!(table_exists(&conn, &app_id, "legacy").await, ".sql table must exist");
    assert!(table_exists(&conn, &app_id, "modern").await, ".ir.json table must exist");
    // ONE ordered timeline: the .sql (Flyway loader) and the .ir.json (IR gate) both
    // applied in this single deploy — the platform `.sql` set and the creator IR set
    // load + apply together, not as two disjoint passes. `applied` is non-empty (the
    // deploy did real work), and BOTH tables exist (asserted above).
    assert!(
        !outcome.applied.is_empty(),
        "the mixed-history deploy applied the .sql and .ir.json set in one timeline"
    );

    let _ = std::fs::remove_dir_all(&dir);
    cleanup_app(&conn, &app_id).await;
}

// PR7 EVIDENCE (PG leg) — the headline "op.* replaces raw-SQL authoring" proof: a
// DDL+backfill migration authored ENTIRELY as op.* `.ir.json` (the §3.1 hero shape —
// addColumn first_name/last_name + a splitPart backfill + dropColumn name) applies
// through the REAL deploy entry point `apply_bundle_migrations` with NO raw `.sql`
// anywhere in the bundle. The migrations dir contains only `.ir.json` files; the test
// ASSERTS that (no `.sql`), then applies and verifies the split transform — the
// concrete evidence a portable bi-dialect data migration needs no hand-written SQL.
#[compio::test]
async fn deploy_migrate_no_raw_sql_hero_ddl_backfill_applies_pg() {
    let Some(conn) = admin_conn().await else {
        eprintln!("SKIP: zeroship_migrate_test :5440 unreachable");
        return;
    };
    let app_id = fresh_app_id();
    cleanup_app(&conn, &app_id).await;

    let create = r#"{"ir_version":1,"name":"create_people","ops":[
        {"op":"createTable","name":"people","columns":[{"name":"name","type":"text"}]}
    ]}"#;
    // The hero DDL+backfill, op.*-authored: add first_name/last_name, splitPart-backfill
    // from `name`, drop `name`. The backfill needs approval (mutates data), so this is
    // deployed through the approved surface — but it carries NO raw SQL.
    let hero = r#"{"ir_version":1,"name":"split_name","ops":[
        {"op":"addColumn","table":"people","column":"first_name","type":"text"},
        {"op":"addColumn","table":"people","column":"last_name","type":"text"},
        {"op":"backfill","table":"people","cursorColumn":"id","batchSize":50,
         "set":{
            "first_name":{"node":"fnSynth","fn":"splitPart","args":[
                {"node":"colRef","name":"name"},{"node":"literal","value":" "},{"node":"literal","value":1}]},
            "last_name":{"node":"fnSynth","fn":"splitPart","args":[
                {"node":"colRef","name":"name"},{"node":"literal","value":" "},{"node":"literal","value":2}]}
         },"name":"split_name_bf"},
        {"op":"dropColumn","table":"people","column":"name"}
    ]}"#;

    // Deploy #1 (routine): createTable + seed (the seed is op.* `insert` — still no SQL).
    let seed = r#"{"ir_version":1,"name":"seed","ops":[
        {"op":"insert","table":"people",
         "columns":["id","created_at","updated_at","version","name"],"rows":[
            ["p1","2026-01-01T00:00:00Z","2026-01-01T00:00:00Z",1,"Ada Lovelace"],
            ["p2","2026-01-01T00:00:00Z","2026-01-01T00:00:00Z",1,"Grace Hopper"]
        ]}
    ]}"#;
    let dir1 = migrations_dir(&[
        ("0001_create_people.ir.json", create),
        ("0002_seed.ir.json", seed),
    ]);
    // EVIDENCE: the bundle contains NO raw `.sql` file.
    assert!(
        std::fs::read_dir(&dir1)
            .unwrap()
            .filter_map(Result::ok)
            .all(|e| !e.file_name().to_string_lossy().ends_with(".sql")),
        "the op.*-authored bundle must contain NO raw .sql file"
    );
    apply_bundle_migrations(&admin_dsn(), &app_id, &dir1)
        .await
        .expect("create+seed (op.* only) must apply");

    // Deploy #2 (approved): the hero DDL+backfill, op.* only.
    let dir2 = migrations_dir(&[("0003_split_name.ir.json", hero)]);
    assert!(
        std::fs::read_dir(&dir2)
            .unwrap()
            .filter_map(Result::ok)
            .all(|e| !e.file_name().to_string_lossy().ends_with(".sql")),
        "the hero bundle must contain NO raw .sql file"
    );
    apply_bundle_migrations_approved(&admin_dsn(), &app_id, &dir2)
        .await
        .expect("the op.*-authored DDL+backfill hero must apply with no raw SQL");

    // The split transform ran and `name` is gone — proof the op.* path replaced raw SQL.
    let schema = app_id.to_string();
    let rows = conn
        .query(
            &format!("SELECT first_name, last_name FROM \"{schema}\".people ORDER BY id"),
            &[],
        )
        .await
        .expect("read split columns");
    let got: Vec<(Option<String>, Option<String>)> =
        rows.iter().map(|r| (r.get(0), r.get(1))).collect();
    assert_eq!(
        got,
        vec![
            (Some("Ada".to_string()), Some("Lovelace".to_string())),
            (Some("Grace".to_string()), Some("Hopper".to_string())),
        ],
        "the op.* splitPart backfill split the names on the real deploy path"
    );
    assert!(
        !column_exists(&conn, &app_id, "people", "name").await,
        "the op.* dropColumn removed `name`"
    );

    let _ = std::fs::remove_dir_all(&dir1);
    let _ = std::fs::remove_dir_all(&dir2);
    cleanup_app(&conn, &app_id).await;
}

// HOSTILE #1 — a FUTURE `ir_version` is refused by the load gate (an older engine
// never mis-applies a newer artifact). Fires `DeployMigrateError::Ir`; nothing
// applied.
#[compio::test]
async fn deploy_migrate_refuses_future_ir_version() {
    let Some(conn) = admin_conn().await else {
        eprintln!("SKIP: zeroship_migrate_test :5440 unreachable");
        return;
    };
    let app_id = fresh_app_id();
    cleanup_app(&conn, &app_id).await;

    let ir = r#"{"ir_version":999999,"name":"from_the_future","ops":[
        {"op":"createTable","name":"x","columns":[{"name":"a","type":"text"}]}
    ]}"#;
    let dir = migrations_dir(&[("0001_future.ir.json", ir)]);

    let err = apply_bundle_migrations(&admin_dsn(), &app_id, &dir)
        .await
        .expect_err("a future ir_version must be refused by the load gate");
    assert!(
        matches!(err, DeployMigrateError::Ir { .. }),
        "expected a fail-closed Ir gate error, got {err:?}"
    );
    assert!(!table_exists(&conn, &app_id, "x").await, "nothing applied on a refused gate");

    let _ = std::fs::remove_dir_all(&dir);
    cleanup_app(&conn, &app_id).await;
}

// HOSTILE #2 — a bare-name `dropIndex` (no table hint) is the cross-tenant
// ownership BYPASS the §8.6 fail-close closes: no name→owner resolver exists, so
// it is refused fail-closed at validate time. Driven through the real deploy
// entrypoint.
#[compio::test]
async fn deploy_migrate_refuses_bare_name_drop_index() {
    let Some(conn) = admin_conn().await else {
        eprintln!("SKIP: zeroship_migrate_test :5440 unreachable");
        return;
    };
    let app_id = fresh_app_id();
    cleanup_app(&conn, &app_id).await;

    // No `table` hint ⇒ the gate cannot resolve the index's owner ⇒ fail-closed.
    let ir = r#"{"ir_version":1,"name":"sneaky","ops":[
        {"op":"dropIndex","name":"some_other_apps_index"}
    ]}"#;
    let dir = migrations_dir(&[("0001_sneaky.ir.json", ir)]);

    let err = apply_bundle_migrations(&admin_dsn(), &app_id, &dir)
        .await
        .expect_err("a bare-name dropIndex must be refused fail-closed");
    assert!(
        matches!(err, DeployMigrateError::Ir { .. }),
        "expected a fail-closed Ir gate error, got {err:?}"
    );

    let _ = std::fs::remove_dir_all(&dir);
    cleanup_app(&conn, &app_id).await;
}

// HOSTILE (MED-1, code-critic) — a `dropIndex` that UNDER-DECLARES uniqueness
// (`unique:false`) on an index that is ACTUALLY UNIQUE in the live catalog must
// STILL be gated destructive/requires_approval and REFUSED under `Approval::None`.
// The deploy path resolves the index's true uniqueness from the introspected live
// schema (`LiveSchema::unique_indexes`) and OR-s it with the hint, so a
// hostile/buggy author cannot bypass the approval gate by lying about the flag.
// Driven through the REAL deploy entrypoint on real PG. Pre-fix the gate trusted
// the (false) hint alone, so the unique index would have dropped SILENTLY.
#[compio::test]
async fn deploy_migrate_refuses_understated_unique_drop_from_live_fact() {
    let Some(conn) = admin_conn().await else {
        eprintln!("SKIP: zeroship_migrate_test :5440 unreachable");
        return;
    };
    let app_id = fresh_app_id();
    cleanup_app(&conn, &app_id).await;

    // 0001 — create a table + a UNIQUE index on it (named so 0002 can target it).
    let create = r#"{"ir_version":1,"name":"create_users","ops":[
        {"op":"createTable","name":"users","columns":[
            {"name":"email","type":"text","nullable":false}
        ]},
        {"op":"createIndex","table":"users","columns":["email"],
         "name":"users_email_uniq","unique":true}
    ]}"#;
    let dir1 = migrations_dir(&[("0001_create_users.ir.json", create)]);
    apply_bundle_migrations(&admin_dsn(), &app_id, &dir1)
        .await
        .expect("0001 createTable + unique createIndex applies");
    assert!(
        index_exists(&conn, &app_id, "users_email_uniq").await,
        "the unique index must exist after 0001"
    );
    let _ = std::fs::remove_dir_all(&dir1);

    // 0002 — a dropIndex that LIES about uniqueness (`unique:false`) on the
    // actually-unique `users_email_uniq`. The live fact must override the hint:
    // destructive ⇒ refused under Approval::None; the index survives.
    let drop = r#"{"ir_version":1,"name":"drop_uniq","ops":[
        {"op":"dropIndex","name":"users_email_uniq","table":"users","unique":false}
    ]}"#;
    let dir2 = migrations_dir(&[("0002_drop_uniq.ir.json", drop)]);
    let err = apply_bundle_migrations(&admin_dsn(), &app_id, &dir2)
        .await
        .expect_err("an understated-unique drop of a LIVE-unique index must be refused");
    assert!(
        matches!(err, DeployMigrateError::Apply(_) | DeployMigrateError::Ir { .. }),
        "expected a destructive-refusal (Apply) or gate (Ir) error, got {err:?}"
    );
    assert!(
        index_exists(&conn, &app_id, "users_email_uniq").await,
        "the unique index must SURVIVE the refused drop (nothing applied)"
    );

    let _ = std::fs::remove_dir_all(&dir2);
    cleanup_app(&conn, &app_id).await;
}

// HOSTILE #3 — an op targeting a table OWNED BY ANOTHER APP is refused by the
// ownership gate. We first create `victim` (owned by app_id), then a SECOND app
// deploys an `.ir.json` dropping a column on `victim` — but it is deployed under a
// DIFFERENT app schema, so `victim` is NOT in its live registry → UNKNOWN_OWNER →
// refused. (Same-schema, a creator owns all its tables; the cross-tenant guarantee
// is the schema boundary + this fail-closed registry check.)
#[compio::test]
async fn deploy_migrate_refuses_op_on_unregistered_table() {
    let Some(conn) = admin_conn().await else {
        eprintln!("SKIP: zeroship_migrate_test :5440 unreachable");
        return;
    };
    let app_id = fresh_app_id();
    cleanup_app(&conn, &app_id).await;

    // The deploying app's schema is EMPTY (no `victim` table), so an op targeting
    // `victim` finds no registry entry → UNKNOWN_OWNER → fail-closed refusal.
    let ir = r#"{"ir_version":1,"name":"steal","ops":[
        {"op":"dropColumn","table":"victim","column":"secret"}
    ]}"#;
    let dir = migrations_dir(&[("0001_steal.ir.json", ir)]);

    let err = apply_bundle_migrations(&admin_dsn(), &app_id, &dir)
        .await
        .expect_err("an op on an unregistered/unowned table must be refused");
    assert!(
        matches!(err, DeployMigrateError::Ir { .. }),
        "expected a fail-closed Ir ownership error, got {err:?}"
    );

    let _ = std::fs::remove_dir_all(&dir);
    cleanup_app(&conn, &app_id).await;
}

/// The stored `column_default` for `<app_id>.<table>.<col>`, or `None` when the
/// column has no default. PG normalises a string-literal default to
/// `'<value>'::text` (the embedded `;\n` is preserved verbatim inside the literal).
async fn column_default(
    conn: &compio_postgres::Client,
    app_id: &Uuid,
    table: &str,
    col: &str,
) -> Option<String> {
    let schema = app_id.to_string();
    let rows = conn
        .query(
            "SELECT column_default FROM information_schema.columns \
             WHERE table_schema = $1 AND table_name = $2 AND column_name = $3",
            &[&schema, &table, &col],
        )
        .await
        .expect("query information_schema.columns default");
    rows.first().and_then(|r| r.get::<_, Option<String>>("column_default"))
}

// MED (code-critic): a LEGITIMATE portable string-literal column DEFAULT whose
// value CONTAINS the substring `;\n` (and a bare `;`) must deploy CLEANLY through
// the PRODUCTION `.ir.json` guarded deploy path (`apply_bundle_ir_migrations` →
// `load_and_lower_guarded`) on real PG — for BOTH a `createTable` column default
// and an `addColumn` default. Pre-fix the textual `;\n` fragment split broke the
// single CREATE/ADD statement on the literal's interior `;\n`, so the guard denied
// a syntactically-broken half (or `ReassemblyMismatch` tripped) and the valid
// default was non-deployable — a misleading "engine bug". Post-fix the structural
// per-statement fragments keep the literal whole, so the table+column deploy and
// the stored default round-trips with its embedded `;\n` intact. The §6.4 parity
// gate exercises only `lower` (whole-up), never `lower_guarded`, so this fork was
// untested before this case.
#[compio::test]
async fn deploy_migrate_ir_string_default_with_embedded_semicolon_newline_pg() {
    let Some(conn) = admin_conn().await else {
        eprintln!("SKIP: zeroship_migrate_test :5440 unreachable");
        return;
    };
    let app_id = fresh_app_id();
    cleanup_app(&conn, &app_id).await;

    // 0001 createTable with a string default carrying `;\n` (and a bare `;`);
    // 0002 addColumn on the SAME table, also with a `;\n`-bearing string default.
    // The JSON string escapes the newline as `\n`; the value the engine renders is
    // the literal three-byte run `a ; \n b ; c`.
    let create = r#"{"ir_version":1,"name":"create_docs","ops":[
        {"op":"createTable","name":"docs","columns":[
            {"name":"note","type":"text","nullable":false,
             "default":{"literal":{"value":"a;\nb;c"}}}
        ]}
    ]}"#;
    let add = r#"{"ir_version":1,"name":"add_tag","ops":[
        {"op":"addColumn","table":"docs","column":"tag","type":"text",
         "default":{"literal":{"value":"x;\ny"}}}
    ]}"#;
    let dir = migrations_dir(&[
        ("0001_create_docs.ir.json", create),
        ("0002_add_tag.ir.json", add),
    ]);

    let outcome = apply_bundle_migrations(&admin_dsn(), &app_id, &dir)
        .await
        .expect("a portable ;\\n string default must lower + apply on the real deploy path");
    assert!(
        !outcome.applied.is_empty(),
        "the lowered IR migration(s) must apply, got {:?}",
        outcome.applied
    );

    // The table + both columns exist.
    assert!(table_exists(&conn, &app_id, "docs").await, "'docs' must exist");
    assert!(column_exists(&conn, &app_id, "docs", "note").await, "'note' must exist");
    assert!(column_exists(&conn, &app_id, "docs", "tag").await, "'tag' must exist");

    // The stored defaults round-trip with the embedded `;\n` intact (PG renders
    // the literal as `'a;\nb;c'::text`).
    let note_default = column_default(&conn, &app_id, "docs", "note").await;
    assert_eq!(
        note_default.as_deref(),
        Some("'a;\nb;c'::text"),
        "the createTable string default must store its embedded ;\\n verbatim"
    );
    let tag_default = column_default(&conn, &app_id, "docs", "tag").await;
    assert_eq!(
        tag_default.as_deref(),
        Some("'x;\ny'::text"),
        "the addColumn string default must store its embedded ;\\n verbatim"
    );

    let _ = std::fs::remove_dir_all(&dir);
    cleanup_app(&conn, &app_id).await;
}

// ===========================================================================
// renameColumn `.ir.json` THROUGH THE PRODUCTION DEPLOY PATH (MED, code-critic)
// ===========================================================================
//
// The PR2 rename e2e proof lived entirely in direct `IrAuthor::lower_steps` +
// `engine.apply_plan(Approval::Approved)` integration tests, which BYPASS the
// production entry point (`apply_bundle_migrations` → `apply_bundle_ir_migrations`,
// which hard-codes `Approval::None`). Per `feedback_faithful_e2e_tests.md` an e2e
// that never runs the REAL wired path can mask an integration failure — here the
// approval-gate unreachability of an IR rename. These tests drive a `renameColumn`
// `.ir.json` through the SAME code the control deploy handler runs and PIN the
// actual observed contract: a rename — like any approval-gated op — is REFUSED at a
// routine deploy (the PG online expand's backfill needs `Approval::Approved`, which
// the routine path never passes), with NOTHING applied. The out-of-band
// approved-apply surface (not wired in PR2) is the place an approved rename runs;
// this contract test is what would fail RED if a future change wired it.

/// Does column `col` on `<app_id>.<table>` introspect to `data_type` `dt`?
async fn column_has_data_type(
    conn: &compio_postgres::Client,
    app_id: &Uuid,
    table: &str,
    col: &str,
    dt: &str,
) -> bool {
    let schema = app_id.to_string();
    let rows = conn
        .query(
            "SELECT 1 FROM information_schema.columns \
             WHERE table_schema = $1 AND table_name = $2 AND column_name = $3 AND data_type = $4",
            &[&schema, &table, &col, &dt],
        )
        .await
        .expect("query information_schema.columns data_type");
    !rows.is_empty()
}

// MED — a `renameColumn` `.ir.json` is lowered (its IR-vs-live type-gate PASSES
// against the real introspected live `from` column — the deploy-wired
// `table_snapshots` benefit the fix commit added) but is then REFUSED at the
// approval gate of the routine deploy: the production path passes `Approval::None`,
// and the PG online expand's backfill needs `Approval::Approved`. The error is
// `DeployMigrateError::OnlineExpand(OnlineError::Approval)` and NOTHING about the
// rename is applied (the old column is intact, the new column is NOT created — the
// whole expand was refused before any DDL). The rename is deployed as a SEPARATE
// deploy after the createTable so the live snapshot (introspected before the IR
// loop) carries the `from` column — the same shape a real "rename in deploy N+1"
// takes.
#[compio::test]
async fn deploy_migrate_renamecolumn_refused_at_approval_gate_on_routine_deploy() {
    let Some(conn) = admin_conn().await else {
        eprintln!("SKIP: zeroship_migrate_test :5440 unreachable");
        return;
    };
    let app_id = fresh_app_id();
    cleanup_app(&conn, &app_id).await;

    // Deploy #1: create `accounts(email text)`. (A separate deploy so #2's live
    // snapshot includes the `email` column the rename's type-gate reconciles.)
    let create = r#"{"ir_version":1,"name":"create_accounts","ops":[
        {"op":"createTable","name":"accounts","columns":[
            {"name":"email","type":"text","nullable":false}
        ]}
    ]}"#;
    let dir1 = migrations_dir(&[("0001_create_accounts.ir.json", create)]);
    apply_bundle_migrations(&admin_dsn(), &app_id, &dir1)
        .await
        .expect("createTable deploy must succeed");
    assert!(column_exists(&conn, &app_id, "accounts", "email").await, "email created");

    // Deploy #2: renameColumn email → email_address (ty text, matching the live
    // column). The type-gate PASSES (live `text` == IR-derived `text`); the rename
    // lowers to a PG expand-contract; the routine deploy then REFUSES it at the
    // approval gate.
    let rename = r#"{"ir_version":1,"name":"rename_email","ops":[
        {"op":"renameColumn","table":"accounts","from":"email","to":"email_address","type":"text"}
    ]}"#;
    let dir2 = migrations_dir(&[("0002_rename_email.ir.json", rename)]);
    let err = apply_bundle_migrations(&admin_dsn(), &app_id, &dir2)
        .await
        .expect_err("a renameColumn must be refused at the routine deploy's approval gate");
    match err {
        DeployMigrateError::OnlineExpand(zeroship_migrate::OnlineError::Approval) => {}
        other => panic!(
            "expected OnlineExpand(OnlineError::Approval) — the rename's backfill needs \
             Approval::Approved, which the routine deploy never passes; got {other:?}"
        ),
    }

    // NOTHING about the rename was applied: the new column was NOT created and the
    // old column is intact (the whole expand was refused before any DDL).
    assert!(
        column_exists(&conn, &app_id, "accounts", "email").await,
        "the old `email` column is intact — the refused rename touched nothing"
    );
    assert!(
        !column_exists(&conn, &app_id, "accounts", "email_address").await,
        "the new `email_address` column must NOT exist — the rename was refused before any DDL"
    );

    let _ = std::fs::remove_dir_all(&dir1);
    let _ = std::fs::remove_dir_all(&dir2);
    cleanup_app(&conn, &app_id).await;
}

// PR7 ONLINE-RENAME GO-LIVE (PG leg) — a `renameColumn` deploy COMPLETES the EXPAND
// through the REAL approved-apply entry point `apply_bundle_migrations_approved`
// (NOT engine-level lowering): the new column is created + dual-written (the EXPAND
// E1..E3 + backfill applies under `Approval::Approved` and the held project lock),
// existing rows are MIRRORED into the new column, and the CONTRACT (drop the old
// column) is surfaced as `pending_contract` for a later approved deploy — NOT applied
// now (the cross-deploy expand-contract partition, §2.0.2). The OLD column is still
// present (the contract has not run), so app code can migrate from `<from>` to `<to>`
// between the two deploys with zero downtime. This is the deploy-path proof the
// engine's online expand is now go-live-wired, the peer of the routine-deploy refusal
// above.
#[compio::test]
async fn deploy_migrate_renamecolumn_approved_completes_expand_and_surfaces_pending_contract() {
    let Some(conn) = admin_conn().await else {
        eprintln!("SKIP: zeroship_migrate_test :5440 unreachable");
        return;
    };
    let app_id = fresh_app_id();
    cleanup_app(&conn, &app_id).await;

    // Deploy #1 (routine): create `members(handle text)` and seed two rows.
    let create = r#"{"ir_version":1,"name":"create_members","ops":[
        {"op":"createTable","name":"members","columns":[
            {"name":"handle","type":"text","nullable":false}
        ]}
    ]}"#;
    let dir1 = migrations_dir(&[("0001_create_members.ir.json", create)]);
    apply_bundle_migrations(&admin_dsn(), &app_id, &dir1)
        .await
        .expect("createTable deploy must succeed");
    assert!(column_exists(&conn, &app_id, "members", "handle").await, "handle created");

    // Seed rows BEFORE the rename — the EXPAND backfill must mirror them into the
    // new column.
    // The IR `createTable` emits the platform system fields; the NOT-NULL ones
    // (`created_at`/`updated_at`/`version`) have no DB-side default (the runtime
    // stamps them), so seed them explicitly.
    let schema = app_id.to_string();
    conn.batch_execute(&format!(
        "INSERT INTO \"{schema}\".members (id, handle, created_at, updated_at, version) VALUES \
         ('m1','ada',  now(), now(), 1), \
         ('m2','grace',now(), now(), 1)"
    ))
    .await
    .expect("seed members");

    // Deploy #2 (APPROVED go-live): renameColumn handle → username. Through the
    // approved-apply surface the EXPAND completes and the CONTRACT is surfaced as
    // pending.
    let rename = r#"{"ir_version":1,"name":"rename_handle","ops":[
        {"op":"renameColumn","table":"members","from":"handle","to":"username","type":"text"}
    ]}"#;
    let dir2 = migrations_dir(&[("0002_rename_handle.ir.json", rename)]);
    let outcome = apply_bundle_migrations_approved(&admin_dsn(), &app_id, &dir2)
        .await
        .expect("an APPROVED renameColumn deploy must COMPLETE the expand");

    // The CONTRACT (C1/C2 drop old column) is surfaced as pending — NOT applied now.
    assert!(
        !outcome.pending_contract.is_empty(),
        "the completed online-rename EXPAND must surface a pending CONTRACT (the C1/C2 \
         drop-old-column owed to a later approved deploy), got {outcome:?}"
    );

    // The NEW column EXISTS (E1 ADD COLUMN applied) and the OLD column is STILL
    // present (the contract has NOT run — the cross-deploy partition).
    assert!(
        column_exists(&conn, &app_id, "members", "username").await,
        "the new `username` column was created by the completed EXPAND"
    );
    assert!(
        column_exists(&conn, &app_id, "members", "handle").await,
        "the old `handle` column is still present — the CONTRACT is pending, not applied"
    );

    // The EXISTING rows were MIRRORED into the new column by the EXPAND backfill.
    let rows = conn
        .query(
            &format!(
                "SELECT username FROM \"{schema}\".members ORDER BY id"
            ),
            &[],
        )
        .await
        .expect("read username");
    let names: Vec<String> = rows
        .iter()
        .filter_map(|r| r.get::<_, Option<String>>(0))
        .collect();
    assert_eq!(
        names,
        vec!["ada".to_string(), "grace".to_string()],
        "the EXPAND backfill mirrored the existing rows into the new column"
    );

    let _ = std::fs::remove_dir_all(&dir1);
    let _ = std::fs::remove_dir_all(&dir2);
    cleanup_app(&conn, &app_id).await;
}

// MED — a rename whose IR `type` DISAGREES with the live `from` column's actual
// type is refused EARLIER, at the fail-closed LOWER gate (`DeployMigrateError::Ir`
// wrapping `RenameTypeMismatch`), NOT at the approval gate — proving the deploy-
// wired `table_snapshots` reconciliation runs on the production path (the fix
// commit's stated production benefit, previously verified only in direct
// lower_steps tests). The IR claims the renamed column is `int` over a live `text`
// column — a data-corruption-class mismatch that must never lower.
#[compio::test]
async fn deploy_migrate_renamecolumn_type_mismatch_refused_at_lower_gate() {
    let Some(conn) = admin_conn().await else {
        eprintln!("SKIP: zeroship_migrate_test :5440 unreachable");
        return;
    };
    let app_id = fresh_app_id();
    cleanup_app(&conn, &app_id).await;

    let create = r#"{"ir_version":1,"name":"create_people","ops":[
        {"op":"createTable","name":"people","columns":[
            {"name":"name","type":"text","nullable":false}
        ]}
    ]}"#;
    let dir1 = migrations_dir(&[("0001_create_people.ir.json", create)]);
    apply_bundle_migrations(&admin_dsn(), &app_id, &dir1)
        .await
        .expect("createTable deploy must succeed");

    // The live `name` is `text`; the IR claims the renamed column is `int`.
    let rename = r#"{"ir_version":1,"name":"rename_name","ops":[
        {"op":"renameColumn","table":"people","from":"name","to":"full_name","type":"int"}
    ]}"#;
    let dir2 = migrations_dir(&[("0002_rename_name.ir.json", rename)]);
    let err = apply_bundle_migrations(&admin_dsn(), &app_id, &dir2)
        .await
        .expect_err("a rename whose IR type disagrees with the live column must fail closed");
    match err {
        DeployMigrateError::Ir { .. } => {}
        other => panic!(
            "expected a fail-closed Ir lower error (RenameTypeMismatch) before any apply, \
             got {other:?}"
        ),
    }
    assert!(
        !column_exists(&conn, &app_id, "people", "full_name").await,
        "nothing applied — the mismatched rename was refused at the lower gate"
    );

    let _ = std::fs::remove_dir_all(&dir1);
    let _ = std::fs::remove_dir_all(&dir2);
    cleanup_app(&conn, &app_id).await;
}

// LOW — the extension-type round-trip pinned through the PRODUCTION path: a
// `renameColumn` of a `vector(N)` column lowers cleanly (its type-gate reconciles
// the REAL introspected `vector(N)` against the IR-derived `vector(N)` — the
// dimension carried through) and reaches the SAME approval-gate refusal as a base
// type. If the round-trip canonicalisation were asymmetric the rename would instead
// false-reject at the LOWER gate (`DeployMigrateError::Ir`/`RenameTypeMismatch`), so
// observing `OnlineExpand(Approval)` PROVES the gate passed on a live `USER-DEFINED`
// extension column. Skips cleanly when the test DB has no `vector` extension —
// UNLESS `MIGRATE_REQUIRE_VECTOR` is set, in which case a missing `vector`
// extension is a HARD FAILURE (CI hard-gate, mirroring `MIGRATE_REQUIRE_DB`): the
// production-layer vector reconciliation MUST be exercised, not vacuously skipped.
#[compio::test]
async fn deploy_migrate_renamecolumn_vector_type_gate_round_trips_on_production_path() {
    let Some(conn) = admin_conn().await else {
        eprintln!("SKIP: zeroship_migrate_test :5440 unreachable");
        return;
    };
    let has_vector = conn
        .query("SELECT 1 FROM pg_extension WHERE extname = 'vector'", &[])
        .await
        .map(|rows| !rows.is_empty())
        .unwrap_or(false);
    if !has_vector {
        assert!(
            std::env::var("MIGRATE_REQUIRE_VECTOR").is_err(),
            "MIGRATE_REQUIRE_VECTOR is set but the test DB has no `vector` extension; \
             the production-path vector type-gate round-trip must NOT silently skip in CI \
             — use a pgvector image (pgvector/pgvector:pg17) so the reconciliation is \
             actually exercised, not vacuously green"
        );
        eprintln!(
            "SKIP deploy_migrate_renamecolumn_vector_type_gate_round_trips_on_production_path: \
             no `vector` extension on the test DB (use pgvector/pgvector:pg17)"
        );
        return;
    }
    let app_id = fresh_app_id();
    cleanup_app(&conn, &app_id).await;

    // Deploy #1: a table with a `vector(3)` column.
    let create = r#"{"ir_version":1,"name":"create_docs","ops":[
        {"op":"createTable","name":"docs","columns":[
            {"name":"embedding","type":{"vector":3},"nullable":false}
        ]}
    ]}"#;
    let dir1 = migrations_dir(&[("0001_create_docs.ir.json", create)]);
    apply_bundle_migrations(&admin_dsn(), &app_id, &dir1)
        .await
        .expect("createTable with a vector(3) column must succeed");
    assert!(
        column_has_data_type(&conn, &app_id, "docs", "embedding", "USER-DEFINED").await,
        "the vector column introspects as USER-DEFINED (extension type)"
    );

    // Deploy #2: renameColumn embedding → vec (ty vector:3, matching live). The
    // type-gate must reconcile the live `vector(3)` against the IR-derived
    // `vector(3)` — round-trip symmetric — then reach the approval gate.
    let rename = r#"{"ir_version":1,"name":"rename_embedding","ops":[
        {"op":"renameColumn","table":"docs","from":"embedding","to":"vec","type":{"vector":3}}
    ]}"#;
    let dir2 = migrations_dir(&[("0002_rename_embedding.ir.json", rename)]);
    let err = apply_bundle_migrations(&admin_dsn(), &app_id, &dir2)
        .await
        .expect_err("the vector rename reaches the approval gate (the type-gate passed)");
    match err {
        DeployMigrateError::OnlineExpand(zeroship_migrate::OnlineError::Approval) => {}
        DeployMigrateError::Ir { source, .. } => panic!(
            "the vector rename FALSE-REJECTED at the lower type-gate (round-trip asymmetry): \
             {source}"
        ),
        other => panic!("expected OnlineExpand(Approval) after a passing type-gate, got {other:?}"),
    }

    let _ = std::fs::remove_dir_all(&dir1);
    let _ = std::fs::remove_dir_all(&dir2);
    cleanup_app(&conn, &app_id).await;
}

// A path-only reference so an unused-import lint never fires if a test is
// cfg'd out in a future refactor.
#[allow(dead_code)]
fn _path_marker(_: &Path) {}
