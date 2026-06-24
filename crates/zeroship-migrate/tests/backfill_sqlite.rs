//! PR6b — FAITHFUL e2e for the SQLite **batched / resumable backfill executor**
//! (§2.3.1) against REAL temp-file SQLite (no shim, no PG-gating). Drives the real
//! hardened migration connection + the real batched executor, then probes the rows
//! to prove the data transform actually happened, crash-safely and exactly-once.
//!
//! Coverage (the spec's PR6b SQLite backfill obligations):
//! - a large table transforms in bounded batches, resumable;
//! - crash mid-run (bounded run, no completion) → resume reaches the SAME final
//!   state, EXACTLY ONCE (a `val = val + 1` transform makes a double-apply visible);
//! - the cursor advances in NATURAL (numeric) order, never lexical;
//! - a re-run after completion is an idempotent no-op;
//! - approval/cursor-safety gates fire (defense-in-depth).

use std::path::PathBuf;

use tempfile::TempDir;
use zeroship_migrate::backfill::{BackfillError, BackfillSpec};
use zeroship_migrate::backend_sqlite::Mode;
use zeroship_migrate::SqliteBackend;

/// A process-wide lock serializing every backfill test in this file. The
/// crash-fuzz test arms the PROCESS-GLOBAL fault registry
/// (`fault::arm(BACKFILL_MID_BATCHES, …)`), which would trip a CONCURRENT backfill
/// in another test. Holding this lock for each test's backfill run makes the
/// armed-fault window exclusive (the SQLite analog of the `_pg` suite's
/// `--test-threads=1` convention, scoped to this file so the rest of the suite
/// still parallelizes).
fn serial() -> std::sync::MutexGuard<'static, ()> {
    use std::sync::{Mutex, OnceLock};
    static LOCK: OnceLock<Mutex<()>> = OnceLock::new();
    // Recover from a poisoned lock (a panicking test still releases the window).
    LOCK.get_or_init(|| Mutex::new(())).lock().unwrap_or_else(|e| e.into_inner())
}

struct Paths {
    _dir: TempDir,
    app: PathBuf,
    journal: PathBuf,
}

fn paths(tag: &str) -> Paths {
    let dir = tempfile::tempdir().expect("tempdir");
    let app = dir.path().join(format!("zs-{tag}.sqlite"));
    let journal = dir.path().join(format!("zs-{tag}.migrations.sqlite"));
    Paths { _dir: dir, app, journal }
}

fn backend(p: &Paths) -> SqliteBackend {
    SqliteBackend::open(&p.app, &p.journal).expect("open hardened sqlite backend")
}

/// Seed a `nums(id INTEGER PRIMARY KEY, val INTEGER, done INTEGER)` table with
/// `n` rows (val = id, done = 0) directly via the actor's CreatorUp mode (the
/// creator-writable path), each its own autocommit statement.
async fn seed_nums(be: &SqliteBackend, n: i64) {
    let actor = be.actor();
    actor.set_mode(Mode::CreatorUp).await.unwrap();
    actor
        .exec("CREATE TABLE nums (id INTEGER PRIMARY KEY, val INTEGER NOT NULL, done INTEGER NOT NULL DEFAULT 0)")
        .await
        .expect("create nums");
    // Bulk insert in one statement for speed.
    let mut vals = Vec::with_capacity(n as usize);
    for i in 1..=n {
        vals.push(format!("({i}, {i}, 0)"));
    }
    actor
        .exec(&format!("INSERT INTO nums (id, val, done) VALUES {}", vals.join(", ")))
        .await
        .expect("seed rows");
}

/// Read a single integer scalar via a CreatorUp query.
async fn scalar_i64(be: &SqliteBackend, sql: &str) -> i64 {
    let actor = be.actor();
    actor.set_mode(Mode::CreatorUp).await.unwrap();
    let rows = actor.query(sql).await.expect("query");
    rows.first()
        .and_then(|r| r.first())
        .and_then(|c| c.clone())
        .and_then(|s| s.parse().ok())
        .unwrap_or(-1)
}

fn spec(batch: u32) -> BackfillSpec {
    BackfillSpec {
        table: "nums".to_string(),
        cursor_column: "id".to_string(),
        batch_size: batch,
        // val = val + 1: a NON-idempotent transform — a double-apply is VISIBLE
        // (a row would land at val = id + 2). Filter to rows not yet done so the
        // exactly-once property is checkable.
        set_clause: "\"val\" = (\"val\" + 1), \"done\" = 1".to_string(),
        filter: Some("\"done\" = 0".to_string()),
        name: "increment_val".to_string(),
    }
}

// ── happy path: large table, bounded batches, resumable ──────────────────────

#[compio::test]
async fn sqlite_backfill_transforms_large_table_in_batches() {
    let _g = serial();
    let p = paths("bf_happy");
    let be = backend(&p);
    seed_nums(&be, 1000).await;

    let s = spec(100);
    let out = be
        .run_backfill_bounded_sqlite(&s, &s.set_clause, s.filter.as_deref(), "tester", None)
        .await
        .expect("backfill runs to completion");
    assert!(out.complete, "backfill completed");
    assert_eq!(out.batches, 10, "1000 rows / 100 per batch = 10 batches");
    assert_eq!(out.rows_updated, 1000, "every row touched once");
    assert!(!out.resumed, "a fresh run did not resume");

    // Every row incremented EXACTLY once: val = id + 1 for all rows.
    let mismatches = scalar_i64(&be, "SELECT count(*) FROM nums WHERE val <> id + 1").await;
    assert_eq!(mismatches, 0, "every row val == id + 1 (exactly-once)");
    let undone = scalar_i64(&be, "SELECT count(*) FROM nums WHERE done <> 1").await;
    assert_eq!(undone, 0, "every row marked done");
}

// ── CRITICAL: crash-resume exactly-once ──────────────────────────────────────
// Run a bounded number of batches (faithful crash: process stops, NOT marked
// complete), then re-run unbounded and assert it resumes from the committed
// cursor and reaches the same final state with NO double-apply.

#[compio::test]
async fn sqlite_backfill_resumes_exactly_once_after_crash() {
    let _g = serial();
    let p = paths("bf_crash");
    let be = backend(&p);
    seed_nums(&be, 1000).await;
    let s = spec(100);

    // Phase 1 — exactly 3 committed batches (rows 1..300), then "crash".
    let out1 = be
        .run_backfill_bounded_sqlite(&s, &s.set_clause, s.filter.as_deref(), "tester", Some(3))
        .await
        .expect("bounded run");
    assert_eq!(out1.batches, 3, "exactly 3 batches before the crash");
    assert_eq!(out1.rows_updated, 300);
    assert!(!out1.complete, "a crashed run is NOT complete");

    // 300 rows incremented; 700 untouched.
    let done = scalar_i64(&be, "SELECT count(*) FROM nums WHERE done = 1").await;
    assert_eq!(done, 300, "exactly 300 rows committed before the crash");

    // Phase 2 — resume unbounded. MUST resume from cursor 300, NOT restart from 1.
    let out2 = be
        .run_backfill_bounded_sqlite(&s, &s.set_clause, s.filter.as_deref(), "tester", None)
        .await
        .expect("resumed run");
    assert!(out2.resumed, "the re-run resumed from a committed cursor");
    assert_eq!(out2.batches, 7, "remaining 700 rows / 100 = 7 batches");
    assert_eq!(out2.rows_updated, 700, "only the remaining 700 rows touched");
    assert!(out2.complete);

    // The exactly-once proof: EVERY row is val = id + 1 (none twice, none missed).
    let mismatches = scalar_i64(&be, "SELECT count(*) FROM nums WHERE val <> id + 1").await;
    assert_eq!(mismatches, 0, "every row incremented EXACTLY once across the crash");
    let total = scalar_i64(&be, "SELECT count(*) FROM nums WHERE done = 1").await;
    assert_eq!(total, 1000);
}

// ── fault-injected crash mid-run (the executor's own crash seam) ─────────────
// Arm the BACKFILL_MID_BATCHES fault to abort the loop AFTER a committed batch
// (behaviorally identical to a process crash there: the batch's UPDATE + cursor
// advance already COMMITted, but the backfill is NOT marked complete). The resume
// must converge to the same final state, exactly once.

#[compio::test]
async fn sqlite_backfill_fault_injected_crash_then_resume_exactly_once() {
    use zeroship_migrate::fault;

    let _g = serial();
    let p = paths("bf_fault");
    let be = backend(&p);
    seed_nums(&be, 500).await;
    let s = spec(100);

    // Arm a crash to fire after the 2nd committed batch (skip = 1 ⇒ fires on the
    // 2nd trip of the point). The point is tripped once per committed batch.
    fault::disarm_all();
    fault::arm(fault::points::BACKFILL_MID_BATCHES, 1);

    let crashed = be
        .run_backfill_bounded_sqlite(&s, &s.set_clause, s.filter.as_deref(), "tester", None)
        .await;
    fault::disarm_all();
    let err = crashed.expect_err("the armed fault aborts the run");
    assert!(matches!(err, BackfillError::Fault(_)), "{err:?}");

    // Two batches committed before the crash (200 rows), the rest untouched.
    let done = scalar_i64(&be, "SELECT count(*) FROM nums WHERE done = 1").await;
    assert_eq!(done, 200, "exactly 2 committed batches survived the crash");

    // Resume — converges to the same final state, exactly once.
    let resumed = be
        .run_backfill_bounded_sqlite(&s, &s.set_clause, s.filter.as_deref(), "tester", None)
        .await
        .expect("resume after fault crash");
    assert!(resumed.resumed, "resumed from the committed cursor");
    assert_eq!(resumed.rows_updated, 300, "only the remaining 300 rows");
    assert!(resumed.complete);
    let mismatches = scalar_i64(&be, "SELECT count(*) FROM nums WHERE val <> id + 1").await;
    assert_eq!(mismatches, 0, "every row incremented EXACTLY once across the fault crash");
}

// ── natural (numeric) cursor order, not lexical ──────────────────────────────
// 1000 ids span 1..1000; a lexical cursor would mis-order ("100" < "99"). A small
// batch over the full range, then assert completeness — a lexical bug would skip
// or re-touch rows around the digit-count boundaries.

#[compio::test]
async fn sqlite_backfill_cursor_is_numeric_order() {
    let _g = serial();
    let p = paths("bf_order");
    let be = backend(&p);
    seed_nums(&be, 1000).await;
    let s = spec(7); // an odd small batch to exercise many windows across boundaries

    let out = be
        .run_backfill_bounded_sqlite(&s, &s.set_clause, s.filter.as_deref(), "tester", None)
        .await
        .expect("backfill");
    assert!(out.complete);
    assert_eq!(out.rows_updated, 1000, "no row skipped or double-touched");
    let mismatches = scalar_i64(&be, "SELECT count(*) FROM nums WHERE val <> id + 1").await;
    assert_eq!(mismatches, 0, "numeric-order paging touched every row exactly once");
}

// ── idempotent re-run after completion ───────────────────────────────────────

#[compio::test]
async fn sqlite_backfill_complete_rerun_is_noop() {
    let _g = serial();
    let p = paths("bf_idem");
    let be = backend(&p);
    seed_nums(&be, 50).await;
    let s = spec(100);

    let first = be
        .run_backfill_bounded_sqlite(&s, &s.set_clause, s.filter.as_deref(), "tester", None)
        .await
        .expect("first");
    assert!(first.complete);
    assert_eq!(first.rows_updated, 50);

    // Re-run: a completed backfill is a no-op (no further increments).
    let again = be
        .run_backfill_bounded_sqlite(&s, &s.set_clause, s.filter.as_deref(), "tester", None)
        .await
        .expect("rerun");
    assert!(again.complete);
    assert_eq!(again.batches, 0, "completed backfill re-run runs no batches");
    assert_eq!(again.rows_updated, 0);
    let mismatches = scalar_i64(&be, "SELECT count(*) FROM nums WHERE val <> id + 1").await;
    assert_eq!(mismatches, 0, "no double-apply on re-run");
}

// ── cursor-safety + identifier gates (defense-in-depth) ──────────────────────

#[compio::test]
async fn sqlite_backfill_rejects_nonunique_cursor() {
    let _g = serial();
    let p = paths("bf_nonuniq");
    let be = backend(&p);
    seed_nums(&be, 10).await;
    // Page on `val` — NOT a unique/PK column.
    let mut s = spec(5);
    s.cursor_column = "val".to_string();
    s.set_clause = "\"done\" = 1".to_string();
    let err = be
        .run_backfill_bounded_sqlite(&s, &s.set_clause, s.filter.as_deref(), "tester", None)
        .await
        .unwrap_err();
    assert!(matches!(err, BackfillError::CursorNotUniqueNotNull { .. }), "{err:?}");
}

#[compio::test]
async fn sqlite_backfill_rejects_cursor_mutation() {
    let _g = serial();
    let p = paths("bf_mutcursor");
    let be = backend(&p);
    seed_nums(&be, 10).await;
    let mut s = spec(5);
    // Mutating the paged column (id) breaks the cursor — must be refused.
    s.set_clause = "\"id\" = (\"id\" + 1000)".to_string();
    s.filter = None;
    let err = be
        .run_backfill_bounded_sqlite(&s, &s.set_clause, s.filter.as_deref(), "tester", None)
        .await
        .unwrap_err();
    assert!(matches!(err, BackfillError::CursorColumnMutated { .. }), "{err:?}");
}

#[compio::test]
async fn sqlite_backfill_rejects_schema_qualified_table() {
    let _g = serial();
    let p = paths("bf_qual");
    let be = backend(&p);
    seed_nums(&be, 1).await;
    let mut s = spec(5);
    s.table = "main.nums".to_string();
    let err = be
        .run_backfill_bounded_sqlite(&s, &s.set_clause, s.filter.as_deref(), "tester", None)
        .await
        .unwrap_err();
    assert!(matches!(err, BackfillError::InvalidIdentifier { what: "table", .. }), "{err:?}");
}
