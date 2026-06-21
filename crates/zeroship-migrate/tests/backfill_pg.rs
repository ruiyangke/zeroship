//! Faithful large-table batched / resumable backfill tests against a REAL
//! Postgres (no shims) — design §5 "Large-table backfill".
//!
//! Requires the dedicated database `zeroship_migrate_test` on :5440 (recreated by
//! the test runbook). Each test runs in its OWN meta + project schema (suffixed by
//! a unique token) so the shared DB stays clean and re-runs are independent.
//!
//! These exercise the REAL path: a real table with enough rows to span many
//! batches, the engine's per-batch committed transactions, the meta-schema
//! progress row, crash-resume exactly-once correctness, the guard/role security
//! path, and the approval gate.

use std::time::Duration;

use compio_postgres::Client;
use zeroship_migrate::{
    backfill_progress, list_backfills, provision_migrator, run_backfill, run_backfill_bounded,
    Approval, BackfillError, BackfillSpec, ExecutorConfig,
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

/// A config WITHOUT a migrator role (admin runs the UPDATE) — used by the
/// happy-path / resume tests that focus on batching + cursor correctness.
fn cfg_for(tok: &str) -> ExecutorConfig {
    let mut c = ExecutorConfig::new(format!("prj_{tok}"), format!("proj_{tok}"));
    c.pg.meta_schema = format!("meta_{tok}");
    // A generous statement_timeout so a 1000-row batch never trips it.
    c.pg.statement_timeout = Duration::from_secs(30);
    c
}

/// A config WITH a provisioned least-privilege migrator role — used by the
/// security tests that assert the batch UPDATE runs under line-2 confinement.
async fn cfg_with_role(conn: &Client, tok: &str) -> ExecutorConfig {
    let mut c = cfg_for(tok);
    let role = zeroship_migrate::migrator_role_name(&c.project_id).unwrap();
    c = c.with_migrator_role(role);
    // Provisioning ALTERs the project schema owner, so it must exist first.
    ensure_project_schema(conn, &c).await;
    provision_migrator(conn, &c).await.expect("provision migrator");
    c
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
    let _ = zeroship_migrate::role::deprovision_migrator(conn, cfg).await;
}

/// Seed a table `widgets(id int PK, val int, normalized text)` with `n` rows,
/// `val = id`, `normalized = NULL`.
async fn seed_widgets(conn: &Client, schema: &str, n: i64) {
    conn.batch_execute(&format!(
        "CREATE TABLE \"{schema}\".widgets (
            id INTEGER PRIMARY KEY,
            val INTEGER NOT NULL,
            normalized TEXT
        )"
    ))
    .await
    .expect("create widgets");
    // Bulk insert via generate_series — id = val = 1..n. `n` is a trusted test
    // fixture count, inlined so generate_series infers int4 (matching the column).
    conn.batch_execute(&format!(
        "INSERT INTO \"{schema}\".widgets (id, val) \
         SELECT g, g FROM generate_series(1, {n}) AS g"
    ))
    .await
    .expect("seed widgets");
}

async fn count_where(conn: &Client, schema: &str, predicate: &str) -> i64 {
    conn.query_one(
        &format!("SELECT count(*) AS c FROM \"{schema}\".widgets WHERE {predicate}"),
        &[],
    )
    .await
    .expect("count")
    .get::<_, i64>("c")
}

// ---------------------------------------------------------------------------
// Happy path: all rows filled, in separate committed batches.
// ---------------------------------------------------------------------------

#[compio::test]
async fn backfill_fills_all_rows_in_ten_separate_batches() {
    let conn = pg().await;
    let tok = token();
    let cfg = cfg_for(&tok);
    drop_schemas(&conn, &cfg).await;
    ensure_project_schema(&conn, &cfg).await;
    seed_widgets(&conn, &cfg.project_schema, 10_000).await;

    let spec = BackfillSpec {
        table: "widgets".into(),
        cursor_column: "id".into(),
        batch_size: 1000,
        set_clause: "normalized = 'v' || _bf.val::text".into(),
        filter: None,
        name: "normalize_widgets".into(),
    };

    let out = run_backfill(&conn, &cfg, &spec, Approval::Approved, "test")
        .await
        .expect("backfill");

    // 10_000 / 1000 = 10 batches; every row filled.
    assert_eq!(out.batches, 10, "10_000 rows / 1000 = 10 committed batches");
    assert_eq!(out.rows_updated, 10_000);
    assert!(out.complete, "backfill reached the table tail");
    assert!(!out.resumed, "fresh run did not resume");
    assert_eq!(
        count_where(&conn, &cfg.project_schema, "normalized IS NULL").await,
        0,
        "every row's normalized column is filled"
    );
    assert_eq!(
        count_where(
            &conn,
            &cfg.project_schema,
            "normalized = 'v' || val::text"
        )
        .await,
        10_000,
        "every row got the authored transform exactly once"
    );

    // The progress row reflects completion.
    let prog = backfill_progress(&conn, &cfg, &out.backfill_id)
        .await
        .expect("progress")
        .expect("progress row exists");
    assert!(prog.complete);
    assert_eq!(prog.rows_done, 10_000);
    assert_eq!(prog.batches_done, 10);
    assert_eq!(prog.last_cursor.as_deref(), Some("10000"));

    drop_schemas(&conn, &cfg).await;
}

// ---------------------------------------------------------------------------
// CRITICAL: crash-resume exactly-once. Use `val = val + 1` so a double-apply is
// detectable (a row applied twice would be val+2). Stop after 3 committed
// batches (faithful crash), then re-run and assert resume-from-4 + exactly-once.
// ---------------------------------------------------------------------------

#[compio::test]
async fn backfill_resumes_from_committed_cursor_exactly_once_after_crash() {
    let conn = pg().await;
    let tok = token();
    let cfg = cfg_for(&tok);
    drop_schemas(&conn, &cfg).await;
    ensure_project_schema(&conn, &cfg).await;
    seed_widgets(&conn, &cfg.project_schema, 10_000).await;

    // A DETECTABLE-double-apply transform: increment. A row touched twice would be
    // id + 2 (not id + 1). After the whole backfill, every row must be exactly
    // id + 1.
    let spec = BackfillSpec {
        table: "widgets".into(),
        cursor_column: "id".into(),
        batch_size: 1000,
        set_clause: "val = _bf.val + 1".into(),
        filter: None,
        name: "increment_val".into(),
    };

    // Phase 1 — run exactly 3 committed batches, then "crash" (stop, do not mark
    // complete). This faithfully reproduces the post-crash state: 3 batches'
    // progress committed, the rest untouched.
    let out1 = run_backfill_bounded(&conn, &cfg, &spec, Approval::Approved, "test", Some(3))
        .await
        .expect("bounded backfill (3 batches)");
    assert_eq!(out1.batches, 3, "exactly 3 batches before the crash");
    assert_eq!(out1.rows_updated, 3000);
    assert!(!out1.complete, "crashed run is NOT complete");

    // After 3 batches: ids 1..3000 are val=id+1, ids 3001..10000 still val=id.
    assert_eq!(
        count_where(&conn, &cfg.project_schema, "val = id + 1").await,
        3000,
        "first 3000 rows incremented once"
    );
    assert_eq!(
        count_where(&conn, &cfg.project_schema, "val = id").await,
        7000,
        "remaining 7000 rows untouched"
    );

    // The committed cursor is at 3000 (the resume point).
    let prog_mid = backfill_progress(&conn, &cfg, &out1.backfill_id)
        .await
        .expect("progress")
        .expect("row");
    assert_eq!(prog_mid.last_cursor.as_deref(), Some("3000"));
    assert!(!prog_mid.complete);

    // Phase 2 — re-run unbounded. It MUST resume from cursor 3000 (batch 4), NOT
    // restart from zero (which would double-apply ids 1..3000 to id+2).
    let out2 = run_backfill(&conn, &cfg, &spec, Approval::Approved, "test")
        .await
        .expect("resumed backfill");
    assert!(out2.resumed, "the re-run resumed from a committed cursor");
    assert_eq!(out2.batches, 7, "7 remaining batches (3001..10000)");
    assert_eq!(out2.rows_updated, 7000);
    assert!(out2.complete);

    // EXACTLY-ONCE: every row is val = id + 1. A double-apply on the first 3000
    // would show val = id + 2 (zero rows), and a skip would leave val = id.
    assert_eq!(
        count_where(&conn, &cfg.project_schema, "val = id + 1").await,
        10_000,
        "EVERY row incremented EXACTLY once — no skips, no double-apply"
    );
    assert_eq!(
        count_where(&conn, &cfg.project_schema, "val = id + 2").await,
        0,
        "no row was double-applied"
    );
    assert_eq!(
        count_where(&conn, &cfg.project_schema, "val = id").await,
        0,
        "no row was skipped"
    );

    drop_schemas(&conn, &cfg).await;
}

// ---------------------------------------------------------------------------
// A short final batch (table size not a multiple of batch_size) completes.
// ---------------------------------------------------------------------------

#[compio::test]
async fn backfill_handles_non_multiple_table_size() {
    let conn = pg().await;
    let tok = token();
    let cfg = cfg_for(&tok);
    drop_schemas(&conn, &cfg).await;
    ensure_project_schema(&conn, &cfg).await;
    seed_widgets(&conn, &cfg.project_schema, 2500).await;

    let spec = BackfillSpec {
        table: "widgets".into(),
        cursor_column: "id".into(),
        batch_size: 1000,
        set_clause: "normalized = 'x'".into(),
        filter: None,
        name: "fill".into(),
    };
    let out = run_backfill(&conn, &cfg, &spec, Approval::Approved, "test")
        .await
        .expect("backfill");
    // 1000 + 1000 + 500 = 3 batches.
    assert_eq!(out.batches, 3);
    assert_eq!(out.rows_updated, 2500);
    assert!(out.complete);
    assert_eq!(
        count_where(&conn, &cfg.project_schema, "normalized = 'x'").await,
        2500
    );

    // Idempotent re-run: already complete ⇒ no-op.
    let again = run_backfill(&conn, &cfg, &spec, Approval::Approved, "test")
        .await
        .expect("re-run");
    assert!(again.complete);
    assert_eq!(again.batches, 0, "complete backfill re-run is a no-op");
    assert_eq!(again.rows_updated, 0);

    drop_schemas(&conn, &cfg).await;
}

// ---------------------------------------------------------------------------
// An authored row filter restricts the rows touched (and the cursor still
// advances over the full key space).
// ---------------------------------------------------------------------------

#[compio::test]
async fn backfill_with_filter_touches_only_matching_rows() {
    let conn = pg().await;
    let tok = token();
    let cfg = cfg_for(&tok);
    drop_schemas(&conn, &cfg).await;
    ensure_project_schema(&conn, &cfg).await;
    seed_widgets(&conn, &cfg.project_schema, 3000).await;
    // Pre-fill the even rows; the backfill should only touch the odd ones.
    conn.batch_execute(&format!(
        "UPDATE \"{}\".widgets SET normalized = 'pre' WHERE id % 2 = 0",
        cfg.project_schema
    ))
    .await
    .expect("pre-fill");

    let spec = BackfillSpec {
        table: "widgets".into(),
        cursor_column: "id".into(),
        batch_size: 500,
        set_clause: "normalized = 'new'".into(),
        filter: Some("normalized IS NULL".into()),
        name: "fill_nulls".into(),
    };
    let out = run_backfill(&conn, &cfg, &spec, Approval::Approved, "test")
        .await
        .expect("backfill");
    assert!(out.complete);
    assert_eq!(
        count_where(&conn, &cfg.project_schema, "normalized = 'new'").await,
        1500,
        "only the 1500 odd (NULL) rows got 'new'"
    );
    assert_eq!(
        count_where(&conn, &cfg.project_schema, "normalized = 'pre'").await,
        1500,
        "the pre-filled even rows are untouched"
    );

    drop_schemas(&conn, &cfg).await;
}

// ---------------------------------------------------------------------------
// Observability: progress is visible mid-flight and via the project-wide list.
// ---------------------------------------------------------------------------

#[compio::test]
async fn backfill_progress_is_observable_midflight_and_listed() {
    let conn = pg().await;
    let tok = token();
    let cfg = cfg_for(&tok);
    drop_schemas(&conn, &cfg).await;
    ensure_project_schema(&conn, &cfg).await;
    seed_widgets(&conn, &cfg.project_schema, 5000).await;

    let spec = BackfillSpec {
        table: "widgets".into(),
        cursor_column: "id".into(),
        batch_size: 1000,
        set_clause: "normalized = 'x'".into(),
        filter: None,
        name: "observe".into(),
    };

    // Run 2 batches, then observe the mid-flight progress (not complete).
    let mid = run_backfill_bounded(&conn, &cfg, &spec, Approval::Approved, "test", Some(2))
        .await
        .expect("2 batches");
    assert!(!mid.complete);

    let prog = backfill_progress(&conn, &cfg, &mid.backfill_id)
        .await
        .expect("progress")
        .expect("row");
    assert!(!prog.complete, "mid-flight progress is not complete");
    assert_eq!(prog.last_cursor.as_deref(), Some("2000"));
    assert_eq!(prog.rows_done, 2000);
    assert_eq!(prog.batches_done, 2);

    // Listed at the project level.
    let all = list_backfills(&conn, &cfg).await.expect("list");
    assert_eq!(all.len(), 1);
    assert_eq!(all[0].backfill_id, mid.backfill_id);
    assert_eq!(all[0].name, "observe");
    assert!(!all[0].complete);

    // Finish it, then progress reports complete.
    let done = run_backfill(&conn, &cfg, &spec, Approval::Approved, "test")
        .await
        .expect("finish");
    assert!(done.complete);
    assert!(done.resumed);
    let fin = backfill_progress(&conn, &cfg, &done.backfill_id)
        .await
        .expect("progress")
        .expect("row");
    assert!(fin.complete);
    assert_eq!(fin.rows_done, 5000);

    drop_schemas(&conn, &cfg).await;
}

// ---------------------------------------------------------------------------
// SECURITY: approval gate.
// ---------------------------------------------------------------------------

#[compio::test]
async fn backfill_without_approval_is_refused() {
    let conn = pg().await;
    let tok = token();
    let cfg = cfg_for(&tok);
    drop_schemas(&conn, &cfg).await;
    ensure_project_schema(&conn, &cfg).await;
    seed_widgets(&conn, &cfg.project_schema, 10).await;

    let spec = BackfillSpec {
        table: "widgets".into(),
        cursor_column: "id".into(),
        batch_size: 5,
        set_clause: "normalized = 'x'".into(),
        filter: None,
        name: "fill".into(),
    };
    let err = run_backfill(&conn, &cfg, &spec, Approval::None, "test")
        .await
        .expect_err("must refuse without approval");
    assert!(matches!(err, BackfillError::ApprovalRequired), "got {err:?}");
    // Nothing ran.
    assert_eq!(
        count_where(&conn, &cfg.project_schema, "normalized IS NULL").await,
        10,
        "no row was touched without approval"
    );

    drop_schemas(&conn, &cfg).await;
}

// ---------------------------------------------------------------------------
// SECURITY: a dangerous / cross-schema authored transform is guard-denied; the
// whole backfill aborts and nothing is mutated.
// ---------------------------------------------------------------------------

#[compio::test]
async fn backfill_with_cross_schema_transform_is_guard_denied() {
    let conn = pg().await;
    let tok = token();
    let cfg = cfg_for(&tok);
    drop_schemas(&conn, &cfg).await;
    ensure_project_schema(&conn, &cfg).await;
    seed_widgets(&conn, &cfg.project_schema, 10).await;

    // A transform that reaches the platform `control` schema via a subquery —
    // the assembled UPDATE is guard-checked and the cross-schema ref is denied.
    let spec = BackfillSpec {
        table: "widgets".into(),
        cursor_column: "id".into(),
        batch_size: 5,
        set_clause: "normalized = (SELECT email FROM control.users LIMIT 1)".into(),
        filter: None,
        name: "exfiltrate".into(),
    };
    let err = run_backfill(&conn, &cfg, &spec, Approval::Approved, "test")
        .await
        .expect_err("cross-schema transform must be guard-denied");
    assert!(matches!(err, BackfillError::Guard { .. }), "got {err:?}");
    assert_eq!(
        count_where(&conn, &cfg.project_schema, "normalized IS NULL").await,
        10,
        "guard denial mutated nothing"
    );

    drop_schemas(&conn, &cfg).await;
}

#[compio::test]
async fn backfill_with_file_access_transform_is_guard_denied() {
    let conn = pg().await;
    let tok = token();
    let cfg = cfg_for(&tok);
    drop_schemas(&conn, &cfg).await;
    ensure_project_schema(&conn, &cfg).await;
    seed_widgets(&conn, &cfg.project_schema, 10).await;

    let spec = BackfillSpec {
        table: "widgets".into(),
        cursor_column: "id".into(),
        batch_size: 5,
        set_clause: "normalized = pg_read_file('/etc/passwd')".into(),
        filter: None,
        name: "read_file".into(),
    };
    let err = run_backfill(&conn, &cfg, &spec, Approval::Approved, "test")
        .await
        .expect_err("file-access transform must be guard-denied");
    assert!(matches!(err, BackfillError::Guard { .. }), "got {err:?}");

    drop_schemas(&conn, &cfg).await;
}

// ---------------------------------------------------------------------------
// SECURITY: injection via the table / cursor_column identifiers is rejected
// before any SQL is assembled.
// ---------------------------------------------------------------------------

#[compio::test]
async fn backfill_rejects_injection_in_table_identifier() {
    let conn = pg().await;
    let tok = token();
    let cfg = cfg_for(&tok);
    drop_schemas(&conn, &cfg).await;
    ensure_project_schema(&conn, &cfg).await;
    seed_widgets(&conn, &cfg.project_schema, 10).await;

    // A table name carrying an injection / schema qualification.
    for bad in ["widgets\"; DROP TABLE widgets; --", "control.users", "widgets; --", ""] {
        let spec = BackfillSpec {
            table: bad.into(),
            cursor_column: "id".into(),
            batch_size: 5,
            set_clause: "normalized = 'x'".into(),
            filter: None,
            name: "inject".into(),
        };
        let err = run_backfill(&conn, &cfg, &spec, Approval::Approved, "test")
            .await
            .expect_err("injection in table identifier must be rejected");
        assert!(
            matches!(err, BackfillError::InvalidIdentifier { what: "table", .. }),
            "table={bad:?} got {err:?}"
        );
    }
    // The widgets table still exists + untouched (the DROP injection never ran).
    assert_eq!(
        count_where(&conn, &cfg.project_schema, "normalized IS NULL").await,
        10
    );

    drop_schemas(&conn, &cfg).await;
}

#[compio::test]
async fn backfill_rejects_injection_in_cursor_column_identifier() {
    let conn = pg().await;
    let tok = token();
    let cfg = cfg_for(&tok);
    drop_schemas(&conn, &cfg).await;
    ensure_project_schema(&conn, &cfg).await;
    seed_widgets(&conn, &cfg.project_schema, 10).await;

    let spec = BackfillSpec {
        table: "widgets".into(),
        cursor_column: "id\" > 0 OR (SELECT 1)=1 --".into(),
        batch_size: 5,
        set_clause: "normalized = 'x'".into(),
        filter: None,
        name: "inject".into(),
    };
    let err = run_backfill(&conn, &cfg, &spec, Approval::Approved, "test")
        .await
        .expect_err("injection in cursor_column must be rejected");
    assert!(
        matches!(err, BackfillError::InvalidIdentifier { what: "cursor_column", .. }),
        "got {err:?}"
    );

    drop_schemas(&conn, &cfg).await;
}

// ---------------------------------------------------------------------------
// SECURITY: a backfill targeting a table OUTSIDE the project schema is denied —
// the engine validates the bare identifier (no schema qualification) AND the
// search_path + migrator role confine resolution to the project schema. A table
// that does not exist in the project schema yields TargetNotFound.
// ---------------------------------------------------------------------------

#[compio::test]
async fn backfill_targeting_table_outside_project_schema_is_denied() {
    let conn = pg().await;
    let tok = token();
    let cfg = cfg_with_role(&conn, &tok).await;
    ensure_project_schema(&conn, &cfg).await;
    // Re-provision so the migrator owns the (now-created) project schema.
    provision_migrator(&conn, &cfg).await.expect("re-provision");
    seed_widgets(&conn, &cfg.project_schema, 10).await;

    // A bare table name that exists in ANOTHER schema (public) but NOT in the
    // project schema: the engine resolves against the project schema only and
    // reports TargetNotFound — it cannot reach the foreign table.
    conn.batch_execute(
        "CREATE TABLE IF NOT EXISTS public.foreign_secrets (id INTEGER PRIMARY KEY, secret TEXT)",
    )
    .await
    .expect("create foreign table");

    let spec = BackfillSpec {
        table: "foreign_secrets".into(),
        cursor_column: "id".into(),
        batch_size: 5,
        set_clause: "secret = 'pwned'".into(),
        filter: None,
        name: "reach_out".into(),
    };
    let err = run_backfill(&conn, &cfg, &spec, Approval::Approved, "test")
        .await
        .expect_err("a table outside the project schema must not be reachable");
    assert!(
        matches!(err, BackfillError::TargetNotFound(_)),
        "got {err:?}"
    );

    let _ = conn.batch_execute("DROP TABLE IF EXISTS public.foreign_secrets").await;
    drop_schemas(&conn, &cfg).await;
}

// ---------------------------------------------------------------------------
// The batch UPDATE runs under the least-privilege migrator role (line-2): a
// full backfill completes against the migrator-owned project table.
// ---------------------------------------------------------------------------

#[compio::test]
async fn backfill_runs_under_migrator_role_and_completes() {
    let conn = pg().await;
    let tok = token();
    let cfg = cfg_with_role(&conn, &tok).await;
    ensure_project_schema(&conn, &cfg).await;
    provision_migrator(&conn, &cfg).await.expect("re-provision");
    seed_widgets(&conn, &cfg.project_schema, 2000).await;
    // The migrator must own the table it updates; the admin created it, so grant.
    let role = cfg.pg.migrator_role.clone().unwrap();
    conn.batch_execute(&format!(
        "ALTER TABLE \"{}\".widgets OWNER TO \"{}\"",
        cfg.project_schema, role
    ))
    .await
    .expect("chown widgets to migrator");

    let spec = BackfillSpec {
        table: "widgets".into(),
        cursor_column: "id".into(),
        batch_size: 1000,
        set_clause: "normalized = 'ok'".into(),
        filter: None,
        name: "fill".into(),
    };
    let out = run_backfill(&conn, &cfg, &spec, Approval::Approved, "test")
        .await
        .expect("backfill under migrator role");
    assert!(out.complete);
    assert_eq!(out.batches, 2);
    assert_eq!(
        count_where(&conn, &cfg.project_schema, "normalized = 'ok'").await,
        2000
    );
    // The session must NOT be left as the migrator (role restored).
    let cur: String = conn
        .query_one("SELECT current_user AS u", &[])
        .await
        .expect("current_user")
        .get("u");
    assert_ne!(cur, role, "the migrator role must not leak onto the session");

    drop_schemas(&conn, &cfg).await;
}

// ---------------------------------------------------------------------------
// DATA-INTEGRITY: the cursor column must be a single-column UNIQUE/PK key and
// NOT NULL. A non-unique cursor would over-match (`cursor IN (window keys)`
// catches every row sharing a key → defeats the batch bound + bypasses the
// authored filter); a NULL key is silently skipped. Both refused in pre-flight.
// ---------------------------------------------------------------------------

async fn count_null(conn: &Client, schema: &str, table: &str) -> i64 {
    conn.query_one(
        &format!("SELECT count(*) AS c FROM \"{schema}\".{table} WHERE normalized IS NULL"),
        &[],
    )
    .await
    .expect("count")
    .get::<_, i64>("c")
}

#[compio::test]
async fn backfill_refuses_non_unique_cursor_column() {
    let conn = pg().await;
    let tok = token();
    let cfg = cfg_for(&tok);
    drop_schemas(&conn, &cfg).await;
    ensure_project_schema(&conn, &cfg).await;
    // `grp` is NOT NULL but has NO unique index (deliberately duplicate values),
    // so it is unsafe to page on.
    conn.batch_execute(&format!(
        "CREATE TABLE \"{}\".items (id INTEGER PRIMARY KEY, grp INTEGER NOT NULL, normalized TEXT)",
        cfg.project_schema
    ))
    .await
    .expect("create items");
    conn.batch_execute(&format!(
        "INSERT INTO \"{}\".items (id, grp) SELECT g, g % 3 FROM generate_series(1, 30) AS g",
        cfg.project_schema
    ))
    .await
    .expect("seed items");

    let spec = BackfillSpec {
        table: "items".into(),
        cursor_column: "grp".into(),
        batch_size: 10,
        set_clause: "normalized = 'x'".into(),
        filter: None,
        name: "bad_cursor".into(),
    };
    let err = run_backfill(&conn, &cfg, &spec, Approval::Approved, "test")
        .await
        .expect_err("a non-unique cursor column must be refused");
    assert!(
        matches!(&err, BackfillError::CursorNotUniqueNotNull { cursor_column, .. } if cursor_column == "grp"),
        "got {err:?}"
    );
    assert_eq!(
        count_null(&conn, &cfg.project_schema, "items").await,
        30,
        "the refusal mutated nothing"
    );

    drop_schemas(&conn, &cfg).await;
}

#[compio::test]
async fn backfill_refuses_nullable_cursor_column() {
    let conn = pg().await;
    let tok = token();
    let cfg = cfg_for(&tok);
    drop_schemas(&conn, &cfg).await;
    ensure_project_schema(&conn, &cfg).await;
    // `code` is UNIQUE but NULLABLE — NULL-keyed rows would be silently skipped,
    // so it is unsafe to page on even though it is unique.
    conn.batch_execute(&format!(
        "CREATE TABLE \"{}\".things (id INTEGER PRIMARY KEY, code INTEGER UNIQUE, normalized TEXT)",
        cfg.project_schema
    ))
    .await
    .expect("create things");
    conn.batch_execute(&format!(
        "INSERT INTO \"{}\".things (id, code) SELECT g, g FROM generate_series(1, 20) AS g",
        cfg.project_schema
    ))
    .await
    .expect("seed things");

    let spec = BackfillSpec {
        table: "things".into(),
        cursor_column: "code".into(),
        batch_size: 10,
        set_clause: "normalized = 'x'".into(),
        filter: None,
        name: "nullable_cursor".into(),
    };
    let err = run_backfill(&conn, &cfg, &spec, Approval::Approved, "test")
        .await
        .expect_err("a nullable cursor column must be refused");
    assert!(
        matches!(&err, BackfillError::CursorNotUniqueNotNull { cursor_column, reason, .. }
            if cursor_column == "code" && reason.contains("nullable")),
        "got {err:?}"
    );
    assert_eq!(
        count_null(&conn, &cfg.project_schema, "things").await,
        20,
        "the refusal mutated nothing"
    );

    drop_schemas(&conn, &cfg).await;
}

// ---------------------------------------------------------------------------
// DATA-INTEGRITY: a set_clause that assigns the cursor column itself is refused
// — mutating the paged key breaks the cursor (rows re-enter later windows →
// re-processing / loop / double-apply).
// ---------------------------------------------------------------------------

#[compio::test]
async fn backfill_refuses_set_clause_that_mutates_cursor_column() {
    let conn = pg().await;
    let tok = token();
    let cfg = cfg_for(&tok);
    drop_schemas(&conn, &cfg).await;
    ensure_project_schema(&conn, &cfg).await;
    seed_widgets(&conn, &cfg.project_schema, 10).await;

    // The transform assigns `id`, the very column it pages on.
    let spec = BackfillSpec {
        table: "widgets".into(),
        cursor_column: "id".into(),
        batch_size: 5,
        set_clause: "id = id + 1000000".into(),
        filter: None,
        name: "mutate_cursor".into(),
    };
    let err = run_backfill(&conn, &cfg, &spec, Approval::Approved, "test")
        .await
        .expect_err("mutating the cursor column must be refused");
    assert!(
        matches!(&err, BackfillError::CursorColumnMutated { cursor_column } if cursor_column == "id"),
        "got {err:?}"
    );
    // Nothing ran: no id was shifted into the +1_000_000 range.
    assert_eq!(
        count_where(&conn, &cfg.project_schema, "id > 1000").await,
        0,
        "the refusal mutated nothing"
    );

    drop_schemas(&conn, &cfg).await;
}
