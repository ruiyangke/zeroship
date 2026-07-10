#![cfg(feature = "native-pg")]
//! Preconditions — state/data-conditional apply (v3 Plan D). Faithful tests
//! against a REAL Postgres (no shims).
//!
//! Two halves:
//!   - **structured checks** (TableExists/NotExists, ColumnExists/NotExists,
//!     RowCount) evaluated as engine-built parameterized catalog queries, plus
//!     identifier-injection rejection;
//!   - **SqlBoolean + executor integration** (Halt/Skip semantics, guard/role
//!     confinement, single-SELECT shape gate, depends_on-of-a-skipped).
//!
//! Requires the dedicated database `zeroship_migrate_test` on :5440 (recreated by
//! the runbook) and a connecting role with `CREATEROLE` (for the migrator-role
//! tests). Each test runs in its own uniquely-suffixed meta + project schema (and
//! a unique project id → unique migrator role), so the shared DB stays clean.

use compio_postgres::Client;
use zeroship_migrate::apply::executor::ApplyError;
use zeroship_migrate::model::migration::Checksum;
use zeroship_migrate::model::precondition::{CmpOp, Precondition, PreconditionCheck};
use zeroship_migrate::{
    apply, applied, ensure_journal, evaluate_precondition as evaluate, migrator_role_name,
    provision_migrator, apply::role::deprovision_migrator, Approval, ExecutorConfig, Migration,
    MigrationFlags, MigrationId, PreconditionError,
};

const DEFAULT_DSN: &str =
    "host=localhost port=5440 user=postgres password=zeroship dbname=zeroship_migrate_test";

fn dsn() -> String {
    std::env::var("MIGRATE_TEST_DB").unwrap_or_else(|_| DEFAULT_DSN.to_string())
}

async fn pg() -> Client {
    let (client, conn) = compio_postgres::connect(&dsn(), compio_postgres::NoTls)
        .await
        .expect("connect to zeroship_migrate_test on :5440");
    compio::runtime::spawn(async move {
        let _ = conn.run().await;
    })
    .detach();
    client
}

fn token() -> String {
    use std::sync::atomic::{AtomicU64, Ordering};
    static N: AtomicU64 = AtomicU64::new(0);
    let n = N.fetch_add(1, Ordering::SeqCst);
    let pid = std::process::id();
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_nanos();
    format!("{pid}_{nanos}_{n}")
}

fn cfg_for(tok: &str) -> ExecutorConfig {
    let mut c = ExecutorConfig::new(format!("prj_{tok}"), format!("proj_{tok}"));
    c.pg.meta_schema = format!("meta_{tok}");
    c
}

/// A cfg whose apply runs under the provisioned least-privilege migrator role.
fn cfg_with_role(tok: &str) -> ExecutorConfig {
    let c = cfg_for(tok);
    let role = migrator_role_name(&c.project_id).unwrap();
    c.with_migrator_role(role)
}

async fn ensure_project_schema(conn: &Client, cfg: &ExecutorConfig) {
    conn.batch_execute(&format!(
        "CREATE SCHEMA IF NOT EXISTS \"{}\"",
        cfg.project_schema
    ))
    .await
    .expect("create project schema");
}

async fn drop_schemas(conn: &Client, cfg: &ExecutorConfig) {
    let _ = conn
        .batch_execute(&format!(
            "DROP SCHEMA IF EXISTS \"{}\" CASCADE; DROP SCHEMA IF EXISTS \"{}\" CASCADE;",
            cfg.project_schema, cfg.pg.meta_schema
        ))
        .await;
}

async fn teardown_role(conn: &Client, cfg: &ExecutorConfig) {
    let _ = deprovision_migrator(conn, cfg).await;
    drop_schemas(conn, cfg).await;
}

/// A migration with the given preconditions + a correct checksum.
fn mig_with_pre(
    version: MigrationId,
    name: &str,
    up: &str,
    pre: Vec<PreconditionCheck>,
) -> Migration {
    let mut __mig = Migration {
        version,
        name: name.to_string(),
        up: up.to_string(),
        down: None,
        checksum: Checksum::of(&zeroship_migrate::ChecksumInput { up: "", down: None, flags: &MigrationFlags::default(), owner_app: "", depends_on: &[], supersedes: &[], preconditions: &[] }),
        flags: MigrationFlags::default(),
        owner_app: "app_test".to_string(),
        depends_on: Vec::new(),
        supersedes: Vec::new(),
        preconditions: pre,
        existence_guard: None,
    };
    __mig.recompute_checksum();
    __mig
}

/// Count net-applied (completed) journal rows.
async fn applied_versions(conn: &Client, cfg: &ExecutorConfig) -> Vec<String> {
    let mut v: Vec<String> = applied(conn, cfg)
        .await
        .expect("read journal")
        .into_iter()
        .filter(|e| matches!(e.phase, zeroship_migrate::apply::journal::Phase::Completed))
        .map(|e| e.version)
        .collect();
    v.sort();
    v
}

async fn table_present(conn: &Client, schema: &str, table: &str) -> bool {
    conn.query_one(
        "SELECT EXISTS (SELECT 1 FROM information_schema.tables \
                        WHERE table_schema = $1 AND table_name = $2) AS p",
        &[&schema, &table],
    )
    .await
    .expect("table check")
    .get("p")
}

// ===========================================================================
// COMMIT 1 — structured checks evaluate correctly + identifier injection rejected
// ===========================================================================

#[test]
fn structured_checks_evaluate_on_a_real_table() {
    compio::runtime::Runtime::new().unwrap().block_on(async {
        let tok = token();
        let cfg = cfg_for(&tok);
        let conn = pg().await;
        ensure_project_schema(&conn, &cfg).await;
        ensure_journal(&conn, &cfg).await.expect("journal");

        // A table `widgets(id int, name text)` with two rows.
        conn.batch_execute(&format!(
            "CREATE TABLE \"{s}\".widgets (id int primary key, name text); \
             INSERT INTO \"{s}\".widgets (id, name) VALUES (1,'a'),(2,'b');",
            s = cfg.project_schema
        ))
        .await
        .expect("seed table");

        // TableExists: true for widgets, false for a missing table.
        assert!(evaluate(&conn, &cfg, &Precondition::TableExists { table: "widgets".into() })
            .await
            .unwrap());
        assert!(!evaluate(&conn, &cfg, &Precondition::TableExists { table: "nope".into() })
            .await
            .unwrap());

        // TableNotExists: the inverse.
        assert!(!evaluate(&conn, &cfg, &Precondition::TableNotExists { table: "widgets".into() })
            .await
            .unwrap());
        assert!(evaluate(&conn, &cfg, &Precondition::TableNotExists { table: "nope".into() })
            .await
            .unwrap());

        // ColumnExists / ColumnNotExists.
        assert!(evaluate(
            &conn,
            &cfg,
            &Precondition::ColumnExists { table: "widgets".into(), column: "name".into() }
        )
        .await
        .unwrap());
        assert!(!evaluate(
            &conn,
            &cfg,
            &Precondition::ColumnExists { table: "widgets".into(), column: "missing".into() }
        )
        .await
        .unwrap());
        assert!(evaluate(
            &conn,
            &cfg,
            &Precondition::ColumnNotExists { table: "widgets".into(), column: "missing".into() }
        )
        .await
        .unwrap());

        // RowCount across every operator (2 rows).
        for (op, val, want) in [
            (CmpOp::Eq, 2, true),
            (CmpOp::Eq, 3, false),
            (CmpOp::Ne, 3, true),
            (CmpOp::Lt, 3, true),
            (CmpOp::Le, 2, true),
            (CmpOp::Gt, 1, true),
            (CmpOp::Ge, 2, true),
            (CmpOp::Gt, 2, false),
        ] {
            let got = evaluate(
                &conn,
                &cfg,
                &Precondition::RowCount { table: "widgets".into(), op, value: val },
            )
            .await
            .unwrap();
            assert_eq!(got, want, "RowCount {op:?} {val} expected {want}");
        }

        drop_schemas(&conn, &cfg).await;
    });
}

#[test]
fn structured_check_rejects_identifier_injection() {
    compio::runtime::Runtime::new().unwrap().block_on(async {
        let tok = token();
        let cfg = cfg_for(&tok);
        let conn = pg().await;
        ensure_project_schema(&conn, &cfg).await;

        // A schema-qualified table name (cross-tenant reach attempt) is rejected
        // BEFORE any query — not silently treated as a literal table name.
        let err = evaluate(
            &conn,
            &cfg,
            &Precondition::TableExists { table: "control.users".into() },
        )
        .await
        .unwrap_err();
        assert!(
            matches!(
                err,
                PreconditionError::InvalidIdentifier { what: "table", .. }
            ),
            "got {err:?}"
        );

        // A quoted-injection table name.
        let err = evaluate(
            &conn,
            &cfg,
            &Precondition::RowCount {
                table: "widgets\"; DROP TABLE x; --".into(),
                op: CmpOp::Eq,
                value: 0,
            },
        )
        .await
        .unwrap_err();
        assert!(matches!(
            err,
            PreconditionError::InvalidIdentifier { what: "table", .. }
        ));

        // An injected COLUMN name.
        let err = evaluate(
            &conn,
            &cfg,
            &Precondition::ColumnExists {
                table: "widgets".into(),
                column: "name\"; DROP TABLE x; --".into(),
            },
        )
        .await
        .unwrap_err();
        assert!(matches!(
            err,
            PreconditionError::InvalidIdentifier { what: "column", .. }
        ));

        drop_schemas(&conn, &cfg).await;
    });
}

// ===========================================================================
// COMMIT 2 — SqlBoolean + executor integration (Halt/Skip)
// ===========================================================================

#[test]
fn halt_precondition_unmet_applies_nothing() {
    compio::runtime::Runtime::new().unwrap().block_on(async {
        let tok = token();
        let cfg = cfg_for(&tok);
        let conn = pg().await;
        ensure_project_schema(&conn, &cfg).await;

        // The migration creates `t`, but is gated on `dep` already existing
        // (TableExists{dep}) — which it does NOT. Halt => the whole apply fails,
        // nothing applied.
        let m = mig_with_pre(
            MigrationId::generate(),
            "create_t",
            &format!("CREATE TABLE \"{}\".t (id int)", cfg.project_schema),
            vec![PreconditionCheck::halt(Precondition::TableExists { table: "dep".into() })],
        );
        let err = apply(&conn, &cfg, std::slice::from_ref(&m), Approval::None, "tester")
            .await
            .unwrap_err();
        let version = m.version.as_str().to_string();
        assert!(
            matches!(err, ApplyError::PreconditionFailed { version: ref v, .. } if *v == version),
            "got {err:?}"
        );
        // Nothing applied: table `t` was NOT created, journal empty.
        assert!(!table_present(&conn, &cfg.project_schema, "t").await);
        assert!(applied_versions(&conn, &cfg).await.is_empty());

        drop_schemas(&conn, &cfg).await;
    });
}

#[test]
fn halt_precondition_met_applies() {
    compio::runtime::Runtime::new().unwrap().block_on(async {
        let tok = token();
        let cfg = cfg_for(&tok);
        let conn = pg().await;
        ensure_project_schema(&conn, &cfg).await;

        // Seed the dependency table the precondition asserts.
        conn.batch_execute(&format!("CREATE TABLE \"{}\".dep (id int)", cfg.project_schema))
            .await
            .expect("seed dep");

        let m = mig_with_pre(
            MigrationId::generate(),
            "create_t",
            &format!("CREATE TABLE \"{}\".t (id int)", cfg.project_schema),
            vec![PreconditionCheck::halt(Precondition::TableExists { table: "dep".into() })],
        );
        let out = apply(&conn, &cfg, std::slice::from_ref(&m), Approval::None, "tester")
            .await
            .expect("apply");
        assert_eq!(out.applied, vec![m.version.as_str().to_string()]);
        assert!(table_present(&conn, &cfg.project_schema, "t").await);

        drop_schemas(&conn, &cfg).await;
    });
}

#[test]
fn skip_precondition_leaves_migration_pending_then_applies_when_met() {
    compio::runtime::Runtime::new().unwrap().block_on(async {
        let tok = token();
        let cfg = cfg_for(&tok);
        let conn = pg().await;
        ensure_project_schema(&conn, &cfg).await;

        // Gated on `dep` existing, with OnUnmet::Skip. `dep` does NOT exist yet.
        let m = mig_with_pre(
            MigrationId::generate(),
            "create_t",
            &format!("CREATE TABLE \"{}\".t (id int)", cfg.project_schema),
            vec![PreconditionCheck::skip(Precondition::TableExists { table: "dep".into() })],
        );

        // Run 1: the precondition is unmet => the migration is SKIPPED. The apply
        // SUCCEEDS (not an error), nothing applied/journaled, table absent.
        let out = apply(&conn, &cfg, std::slice::from_ref(&m), Approval::None, "tester")
            .await
            .expect("run 1 succeeds (skip is not an error)");
        assert!(out.applied.is_empty(), "skipped migration must not apply");
        assert!(!table_present(&conn, &cfg.project_schema, "t").await);
        assert!(applied_versions(&conn, &cfg).await.is_empty(), "skip must not journal");

        // Now satisfy the precondition.
        conn.batch_execute(&format!("CREATE TABLE \"{}\".dep (id int)", cfg.project_schema))
            .await
            .expect("seed dep");

        // Run 2: precondition now met => the (still-pending) migration applies once.
        let out = apply(&conn, &cfg, std::slice::from_ref(&m), Approval::None, "tester")
            .await
            .expect("run 2 applies");
        assert_eq!(out.applied, vec![m.version.as_str().to_string()]);
        assert!(table_present(&conn, &cfg.project_schema, "t").await);

        // Run 3: idempotent no-op (already applied).
        let out = apply(&conn, &cfg, std::slice::from_ref(&m), Approval::None, "tester")
            .await
            .expect("run 3 no-op");
        assert!(out.is_noop());

        drop_schemas(&conn, &cfg).await;
    });
}

#[test]
fn sql_boolean_precondition_gates_correctly() {
    compio::runtime::Runtime::new().unwrap().block_on(async {
        let tok = token();
        let cfg = cfg_for(&tok);
        let conn = pg().await;
        ensure_project_schema(&conn, &cfg).await;

        // Seed a table `staging` with one row, then zero it out across runs.
        conn.batch_execute(&format!(
            "CREATE TABLE \"{s}\".staging (id int); INSERT INTO \"{s}\".staging VALUES (1);",
            s = cfg.project_schema
        ))
        .await
        .expect("seed staging");

        // The migration applies only when staging is empty; OnUnmet::Skip.
        let m = mig_with_pre(
            MigrationId::generate(),
            "promote",
            &format!("CREATE TABLE \"{}\".promoted (id int)", cfg.project_schema),
            vec![PreconditionCheck::skip(Precondition::SqlBoolean {
                sql: "SELECT count(*) = 0 FROM staging".into(),
            })],
        );

        // staging has 1 row => count(*)=0 is FALSE => skipped.
        let out = apply(&conn, &cfg, std::slice::from_ref(&m), Approval::None, "tester")
            .await
            .expect("run 1");
        assert!(out.applied.is_empty());
        assert!(!table_present(&conn, &cfg.project_schema, "promoted").await);

        // Empty staging => precondition now true => applies.
        conn.batch_execute(&format!("DELETE FROM \"{}\".staging", cfg.project_schema))
            .await
            .expect("empty staging");
        let out = apply(&conn, &cfg, std::slice::from_ref(&m), Approval::None, "tester")
            .await
            .expect("run 2");
        assert_eq!(out.applied, vec![m.version.as_str().to_string()]);
        assert!(table_present(&conn, &cfg.project_schema, "promoted").await);

        drop_schemas(&conn, &cfg).await;
    });
}

#[test]
fn sql_boolean_reaching_control_is_guard_denied() {
    compio::runtime::Runtime::new().unwrap().block_on(async {
        let tok = token();
        let cfg = cfg_for(&tok);
        let conn = pg().await;
        ensure_project_schema(&conn, &cfg).await;

        let m = mig_with_pre(
            MigrationId::generate(),
            "leaky",
            &format!("CREATE TABLE \"{}\".t (id int)", cfg.project_schema),
            vec![PreconditionCheck::halt(Precondition::SqlBoolean {
                sql: "SELECT count(*) = 0 FROM control.users".into(),
            })],
        );
        let err = apply(&conn, &cfg, std::slice::from_ref(&m), Approval::None, "tester")
            .await
            .unwrap_err();
        // The cross-schema precondition is denied (surfaced as PreconditionFailed
        // wrapping the guard denial). Nothing applied.
        let version = m.version.as_str().to_string();
        assert!(
            matches!(err, ApplyError::PreconditionFailed { version: ref v, .. } if *v == version),
            "got {err:?}"
        );
        assert!(!table_present(&conn, &cfg.project_schema, "t").await);
        assert!(applied_versions(&conn, &cfg).await.is_empty());

        drop_schemas(&conn, &cfg).await;
    });
}

#[test]
fn sql_boolean_non_select_is_rejected() {
    compio::runtime::Runtime::new().unwrap().block_on(async {
        let tok = token();
        let cfg = cfg_for(&tok);
        let conn = pg().await;
        ensure_project_schema(&conn, &cfg).await;
        conn.batch_execute(&format!("CREATE TABLE \"{}\".t (id int)", cfg.project_schema))
            .await
            .expect("seed t");

        // A non-SELECT (DELETE) precondition would mutate state — rejected before
        // it runs.
        let m = mig_with_pre(
            MigrationId::generate(),
            "danger",
            &format!("CREATE TABLE \"{}\".u (id int)", cfg.project_schema),
            vec![PreconditionCheck::halt(Precondition::SqlBoolean {
                sql: "DELETE FROM t".into(),
            })],
        );
        let err = apply(&conn, &cfg, std::slice::from_ref(&m), Approval::None, "tester")
            .await
            .unwrap_err();
        let version = m.version.as_str().to_string();
        assert!(
            matches!(err, ApplyError::PreconditionFailed { version: ref v, .. } if *v == version),
            "got {err:?}"
        );
        // The DELETE never ran (state unmutated) and `u` was not created.
        assert!(!table_present(&conn, &cfg.project_schema, "u").await);

        // A multi-statement precondition smuggling a second statement.
        let m2 = mig_with_pre(
            MigrationId::generate(),
            "danger2",
            &format!("CREATE TABLE \"{}\".v (id int)", cfg.project_schema),
            vec![PreconditionCheck::halt(Precondition::SqlBoolean {
                sql: "SELECT true; DROP TABLE t".into(),
            })],
        );
        let err = apply(&conn, &cfg, std::slice::from_ref(&m2), Approval::None, "tester")
            .await
            .unwrap_err();
        assert!(matches!(err, ApplyError::PreconditionFailed { .. }), "got {err:?}");
        // `t` still exists (the smuggled DROP never ran).
        assert!(table_present(&conn, &cfg.project_schema, "t").await);

        drop_schemas(&conn, &cfg).await;
    });
}

#[test]
fn sql_boolean_runs_under_migrator_role() {
    compio::runtime::Runtime::new().unwrap().block_on(async {
        let tok = token();
        let cfg = cfg_with_role(&tok);
        let conn = pg().await;
        ensure_project_schema(&conn, &cfg).await;
        provision_migrator(&conn, &cfg).await.expect("provision role");

        // Seed `gate` THROUGH a migration so the migrator role OWNS it (in
        // production every project table is migrator-owned) — the SqlBoolean
        // precondition then reads it under the same role, proving line-2
        // confinement does not break legitimate own-schema reads.
        let seed = mig_with_pre(
            MigrationId::generate(),
            "seed_gate",
            &format!(
                "CREATE TABLE \"{s}\".gate (ready boolean); INSERT INTO \"{s}\".gate VALUES (true);",
                s = cfg.project_schema
            ),
            vec![],
        );
        apply(&conn, &cfg, std::slice::from_ref(&seed), Approval::None, "tester")
            .await
            .expect("seed gate via migration");

        // A SqlBoolean precondition over the project's OWN (migrator-owned) table,
        // evaluated under the migrator role.
        let m = mig_with_pre(
            MigrationId::generate(),
            "go",
            &format!("CREATE TABLE \"{}\".done (id int)", cfg.project_schema),
            vec![PreconditionCheck::halt(Precondition::SqlBoolean {
                sql: "SELECT bool_and(ready) FROM gate".into(),
            })],
        );
        let out = apply(&conn, &cfg, std::slice::from_ref(&m), Approval::None, "tester")
            .await
            .expect("apply under role");
        assert_eq!(out.applied, vec![m.version.as_str().to_string()]);
        assert!(table_present(&conn, &cfg.project_schema, "done").await);

        teardown_role(&conn, &cfg).await;
    });
}

#[test]
fn dependent_of_a_skipped_migration_does_not_run() {
    compio::runtime::Runtime::new().unwrap().block_on(async {
        let tok = token();
        let cfg = cfg_for(&tok);
        let conn = pg().await;
        ensure_project_schema(&conn, &cfg).await;

        // m1 is gated on `dep` (Skip), which does not exist => m1 skipped.
        // m2 depends_on m1 (creates a child referencing m1's table).
        let m1 = mig_with_pre(
            MigrationId::generate(),
            "parent",
            &format!("CREATE TABLE \"{}\".parent (id int primary key)", cfg.project_schema),
            vec![PreconditionCheck::skip(Precondition::TableExists { table: "dep".into() })],
        );
        let mut m2 = mig_with_pre(
            MigrationId::generate(),
            "child",
            &format!(
                "CREATE TABLE \"{s}\".child (id int, parent_id int references \"{s}\".parent(id))",
                s = cfg.project_schema
            ),
            vec![],
        );
        m2.depends_on = vec![m1.version.clone()];
        m2.checksum = Checksum::of(&zeroship_migrate::ChecksumInput::from_migration(&m2));

        // Run 1: m1 skipped; m2 (depends on the not-yet-applied m1) must NOT run.
        let out = apply(&conn, &cfg, &[m1.clone(), m2.clone()], Approval::None, "tester")
            .await
            .expect("run 1");
        assert!(out.applied.is_empty(), "neither m1 (skipped) nor m2 (dep unmet) runs");
        assert!(!table_present(&conn, &cfg.project_schema, "parent").await);
        assert!(!table_present(&conn, &cfg.project_schema, "child").await);

        // Satisfy m1's precondition; both apply in order.
        conn.batch_execute(&format!("CREATE TABLE \"{}\".dep (id int)", cfg.project_schema))
            .await
            .expect("seed dep");
        let out = apply(&conn, &cfg, &[m1.clone(), m2.clone()], Approval::None, "tester")
            .await
            .expect("run 2");
        assert_eq!(
            out.applied,
            vec![m1.version.as_str().to_string(), m2.version.as_str().to_string()]
        );
        assert!(table_present(&conn, &cfg.project_schema, "parent").await);
        assert!(table_present(&conn, &cfg.project_schema, "child").await);

        drop_schemas(&conn, &cfg).await;
    });
}

// ===========================================================================
// COMMIT 1 (hardening) — the SqlBoolean shape gate is the REAL pre-execution
// line: a data-modifying CTE, a locking clause, and a sequence-mutating /
// advisory-lock builtin are all REJECTED before the SQL ever touches the DB.
// READ ONLY is now defense-in-depth, not the sole backstop. Each test proves
// PRE-execution rejection (no mutation / no lock leak), not merely
// READ-ONLY-at-exec.
// ===========================================================================

/// A data-modifying CTE (`WITH x AS (DELETE … RETURNING …) SELECT …`) is
/// rejected by the shape gate before it runs — the `DeleteStmt` hangs off the
/// `SelectStmt`'s `with_clause`, so the old "top node is `SelectStmt`" gate let
/// it through to READ ONLY. We prove the target row is STILL PRESENT afterward.
#[test]
fn sql_boolean_data_modifying_cte_is_rejected_and_never_runs() {
    compio::runtime::Runtime::new().unwrap().block_on(async {
        let tok = token();
        let cfg = cfg_for(&tok);
        let conn = pg().await;
        ensure_project_schema(&conn, &cfg).await;
        conn.batch_execute(&format!(
            "CREATE TABLE \"{s}\".victim (id int); INSERT INTO \"{s}\".victim VALUES (1);",
            s = cfg.project_schema
        ))
        .await
        .expect("seed victim");

        // The precondition's SELECT is fine on its face, but its CTE DELETEs the
        // victim row. Rejected at the gate (before execution).
        let m = mig_with_pre(
            MigrationId::generate(),
            "dml_cte",
            &format!("CREATE TABLE \"{}\".applied (id int)", cfg.project_schema),
            vec![PreconditionCheck::halt(Precondition::SqlBoolean {
                sql: format!(
                    "WITH x AS (DELETE FROM \"{s}\".victim RETURNING id) \
                     SELECT count(*) = 0 FROM x",
                    s = cfg.project_schema
                ),
            })],
        );
        let err = apply(&conn, &cfg, std::slice::from_ref(&m), Approval::None, "tester")
            .await
            .unwrap_err();
        let version = m.version.as_str().to_string();
        assert!(
            matches!(err, ApplyError::PreconditionFailed { version: ref v, .. } if *v == version),
            "got {err:?}"
        );
        // The migration did not apply...
        assert!(!table_present(&conn, &cfg.project_schema, "applied").await);
        // ...and the DELETE never ran — the victim row is still present.
        let n: i64 = conn
            .query_one(
                &format!("SELECT count(*)::bigint AS n FROM \"{}\".victim", cfg.project_schema),
                &[],
            )
            .await
            .expect("count victim")
            .get("n");
        assert_eq!(n, 1, "the CTE's DELETE must NOT have run");

        drop_schemas(&conn, &cfg).await;
    });
}

/// `SELECT … FOR UPDATE` (a non-empty locking clause) is rejected by the shape
/// gate. A locking clause acquires row locks — not a read-only assertion.
#[test]
fn sql_boolean_for_update_locking_clause_is_rejected() {
    compio::runtime::Runtime::new().unwrap().block_on(async {
        let tok = token();
        let cfg = cfg_for(&tok);
        let conn = pg().await;
        ensure_project_schema(&conn, &cfg).await;
        conn.batch_execute(&format!(
            "CREATE TABLE \"{s}\".t (id int); INSERT INTO \"{s}\".t VALUES (1);",
            s = cfg.project_schema
        ))
        .await
        .expect("seed t");

        let m = mig_with_pre(
            MigrationId::generate(),
            "for_update",
            &format!("CREATE TABLE \"{}\".applied (id int)", cfg.project_schema),
            vec![PreconditionCheck::halt(Precondition::SqlBoolean {
                sql: format!(
                    "SELECT count(*) = 0 FROM (SELECT id FROM \"{s}\".t FOR UPDATE) s",
                    s = cfg.project_schema
                ),
            })],
        );
        let err = apply(&conn, &cfg, std::slice::from_ref(&m), Approval::None, "tester")
            .await
            .unwrap_err();
        let version = m.version.as_str().to_string();
        assert!(
            matches!(err, ApplyError::PreconditionFailed { version: ref v, .. } if *v == version),
            "got {err:?}"
        );
        assert!(!table_present(&conn, &cfg.project_schema, "applied").await);

        drop_schemas(&conn, &cfg).await;
    });
}

/// `SELECT nextval('seq')` is rejected by the shape gate — `nextval` mutates the
/// sequence and `READ ONLY` does NOT block it. We prove PRE-execution rejection:
/// the sequence has NOT advanced after the rejected precondition.
#[test]
fn sql_boolean_nextval_is_rejected_and_sequence_does_not_advance() {
    compio::runtime::Runtime::new().unwrap().block_on(async {
        let tok = token();
        let cfg = cfg_for(&tok);
        let conn = pg().await;
        ensure_project_schema(&conn, &cfg).await;
        conn.batch_execute(&format!(
            "CREATE SEQUENCE \"{}\".s START 1",
            cfg.project_schema
        ))
        .await
        .expect("seed seq");

        let m = mig_with_pre(
            MigrationId::generate(),
            "nextval",
            &format!("CREATE TABLE \"{}\".applied (id int)", cfg.project_schema),
            vec![PreconditionCheck::halt(Precondition::SqlBoolean {
                sql: format!("SELECT nextval('\"{}\".s') > 0", cfg.project_schema),
            })],
        );
        let err = apply(&conn, &cfg, std::slice::from_ref(&m), Approval::None, "tester")
            .await
            .unwrap_err();
        let version = m.version.as_str().to_string();
        assert!(
            matches!(err, ApplyError::PreconditionFailed { version: ref v, .. } if *v == version),
            "got {err:?}"
        );
        assert!(!table_present(&conn, &cfg.project_schema, "applied").await);
        // The sequence never advanced: the FIRST real nextval returns 1. (Had the
        // precondition run, this would return 2.)
        let v: i64 = conn
            .query_one(
                &format!("SELECT nextval('\"{}\".s') AS v", cfg.project_schema),
                &[],
            )
            .await
            .expect("nextval")
            .get("v");
        assert_eq!(v, 1, "nextval in the precondition must NOT have advanced the sequence");

        drop_schemas(&conn, &cfg).await;
    });
}

/// `SELECT pg_advisory_lock(1)` is rejected by the shape gate. This is the real
/// gap: `READ ONLY` does NOT block advisory locks, and a SESSION-scoped advisory
/// lock would LEAK onto the pooled connection and could collide with the
/// engine's own apply lock. We prove NO session advisory lock is held on the
/// connection after the rejected precondition.
#[test]
fn sql_boolean_advisory_lock_is_rejected_and_does_not_leak() {
    compio::runtime::Runtime::new().unwrap().block_on(async {
        let tok = token();
        let cfg = cfg_for(&tok);
        let conn = pg().await;
        ensure_project_schema(&conn, &cfg).await;

        let m = mig_with_pre(
            MigrationId::generate(),
            "advisory",
            &format!("CREATE TABLE \"{}\".applied (id int)", cfg.project_schema),
            vec![PreconditionCheck::halt(Precondition::SqlBoolean {
                sql: "SELECT pg_advisory_lock(424242) IS NOT NULL".into(),
            })],
        );
        let err = apply(&conn, &cfg, std::slice::from_ref(&m), Approval::None, "tester")
            .await
            .unwrap_err();
        let version = m.version.as_str().to_string();
        assert!(
            matches!(err, ApplyError::PreconditionFailed { version: ref v, .. } if *v == version),
            "got {err:?}"
        );
        assert!(!table_present(&conn, &cfg.project_schema, "applied").await);
        // No advisory lock leaked onto THIS backend's session.
        let held: i64 = conn
            .query_one(
                "SELECT count(*)::bigint AS n FROM pg_locks \
                 WHERE locktype = 'advisory' AND pid = pg_backend_pid()",
                &[],
            )
            .await
            .expect("advisory lock count")
            .get("n");
        assert_eq!(held, 0, "a session advisory lock must NOT have leaked onto the connection");

        drop_schemas(&conn, &cfg).await;
    });
}

/// The hardened gate does NOT over-reject: a legitimate read-only assertion
/// (`count(*) = 0`) and the read-only sequence builtin `currval` still pass.
#[test]
fn sql_boolean_legitimate_read_only_preconditions_still_pass() {
    compio::runtime::Runtime::new().unwrap().block_on(async {
        let tok = token();
        let cfg = cfg_for(&tok);
        let conn = pg().await;
        ensure_project_schema(&conn, &cfg).await;

        // count(*) = 0 over an empty own-schema table — met, so the migration
        // applies.
        let seed = mig_with_pre(
            MigrationId::generate(),
            "seed_empty",
            &format!("CREATE TABLE \"{}\".staging (id int)", cfg.project_schema),
            vec![],
        );
        apply(&conn, &cfg, std::slice::from_ref(&seed), Approval::None, "tester")
            .await
            .expect("seed staging");

        let m = mig_with_pre(
            MigrationId::generate(),
            "go",
            &format!("CREATE TABLE \"{}\".done (id int)", cfg.project_schema),
            vec![PreconditionCheck::halt(Precondition::SqlBoolean {
                sql: "SELECT count(*) = 0 FROM staging".into(),
            })],
        );
        let out = apply(&conn, &cfg, std::slice::from_ref(&m), Approval::None, "tester")
            .await
            .expect("apply with legit precondition");
        assert_eq!(out.applied, vec![m.version.as_str().to_string()]);
        assert!(table_present(&conn, &cfg.project_schema, "done").await);

        // `currval` is read-only (reads the session's last nextval value) — it is
        // ALLOWED by the gate. We prime it with a nextval in a separate
        // statement, then a precondition reads it back.
        conn.batch_execute(&format!("CREATE SEQUENCE \"{}\".s START 5", cfg.project_schema))
            .await
            .expect("seq");
        let _ = conn
            .query_one(&format!("SELECT nextval('\"{}\".s')", cfg.project_schema), &[])
            .await
            .expect("prime currval");
        // The gate must ALLOW currval (a read-only builtin). It passes the gate;
        // whether it then errors at exec on a fresh pooled connection is a
        // runtime concern, not a gate one — so we assert the gate does not reject
        // it by checking `evaluate` does not return NotABooleanSelect.
        let res = evaluate(
            &conn,
            &cfg,
            &Precondition::SqlBoolean {
                sql: format!("SELECT currval('\"{}\".s') > 0", cfg.project_schema),
            },
        )
        .await;
        assert!(
            !matches!(res, Err(PreconditionError::NotABooleanSelect { .. })),
            "currval is read-only and must pass the shape gate, got {res:?}"
        );

        drop_schemas(&conn, &cfg).await;
    });
}
