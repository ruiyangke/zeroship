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

use zeroship_control::deploy_migrate::{apply_bundle_migrations, DeployMigrateError};

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

    apply_bundle_migrations(&admin_dsn(), &app_id, &dir)
        .await
        .expect("both the .sql and .ir.json must apply");
    assert!(table_exists(&conn, &app_id, "legacy").await, ".sql table must exist");
    assert!(table_exists(&conn, &app_id, "modern").await, ".ir.json table must exist");

    let _ = std::fs::remove_dir_all(&dir);
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

// A path-only reference so an unused-import lint never fires if a test is
// cfg'd out in a future refactor.
#[allow(dead_code)]
fn _path_marker(_: &Path) {}
