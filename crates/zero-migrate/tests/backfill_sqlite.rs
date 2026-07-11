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

// Every test holds the `serial()` `MutexGuard` across its `.await`s ON PURPOSE:
// the guard is a test-only serialization lock over `()` (see `serial`) whose whole
// job is to keep the PROCESS-GLOBAL armed-fault window exclusive for the duration
// of one backfill run. There is no cross-task contention to deadlock (compio test,
// single executor), so `await_holding_lock` is a false positive for this
// deliberate pattern — allowed narrowly, scoped to this one test file.
#![allow(clippy::await_holding_lock)]

use std::path::PathBuf;

use tempfile::TempDir;
use zero_migrate::{apply::backend::BackfillError, BackfillSpec};
use zero_migrate::apply::backend::sqlite::Mode;
use zero_migrate::SqliteBackend;

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
        // SQLite has one `main` db, no schema namespace; the executor renders the
        // table unqualified. The field is carried for `backfill_id` discrimination.
        schema: "main".to_string(),
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
    use zero_migrate::fault;

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

// ── REAL (numeric-affinity, non-integral) unique cursor, exactly-once ────────
// MED (PR6b code-critic): `max_returned_cursor` parsed each RETURNING'd cell with
// `parse::<i64>()` and SILENTLY DROPPED any non-i64 value — so a UNIQUE NOT NULL
// REAL cursor (legal, just unusual) yielded max_cursor=None even with n>0 rows
// touched, writing last_cursor=NULL, re-scanning from the start (WHERE 1=1), and
// re-applying the non-idempotent transform. The bind side already had a text
// fallback; the max side did not. This faithful e2e proves exactly-once over a REAL
// cursor with actual non-integral data, which a re-apply loop would fail (val=id+2
// somewhere) and a premature-stop would fail (a row left at val=id).

/// Seed `rnums(rk REAL UNIQUE NOT NULL, id INTEGER PRIMARY KEY, val INTEGER, done
/// INTEGER)` with `n` rows whose REAL cursor `rk = i + 0.5` is NON-integral (so the
/// `parse::<i64>()` path would drop EVERY cell). `val = id`, `done = 0`.
async fn seed_real_cursor(be: &SqliteBackend, n: i64) {
    let actor = be.actor();
    actor.set_mode(Mode::CreatorUp).await.unwrap();
    actor
        .exec(
            "CREATE TABLE rnums (\
               rk REAL NOT NULL UNIQUE, \
               id INTEGER PRIMARY KEY, \
               val INTEGER NOT NULL, \
               done INTEGER NOT NULL DEFAULT 0)",
        )
        .await
        .expect("create rnums");
    let mut vals = Vec::with_capacity(n as usize);
    for i in 1..=n {
        // rk = i + 0.5 — distinct, NON-integral, numeric-order = id-order.
        vals.push(format!("({i}.5, {i}, {i}, 0)"));
    }
    actor
        .exec(&format!("INSERT INTO rnums (rk, id, val, done) VALUES {}", vals.join(", ")))
        .await
        .expect("seed rnums");
}

fn real_spec(batch: u32) -> BackfillSpec {
    BackfillSpec {
        schema: "main".to_string(),
        table: "rnums".to_string(),
        cursor_column: "rk".to_string(),
        batch_size: batch,
        set_clause: "\"val\" = (\"val\" + 1), \"done\" = 1".to_string(),
        filter: Some("\"done\" = 0".to_string()),
        name: "increment_real".to_string(),
    }
}

#[compio::test]
async fn sqlite_backfill_real_cursor_advances_exactly_once() {
    let _g = serial();
    let p = paths("bf_real");
    let be = backend(&p);
    seed_real_cursor(&be, 50).await;
    let s = real_spec(10); // 5 batches; each must advance the REAL cursor, not reset

    let out = be
        .run_backfill_bounded_sqlite(&s, &s.set_clause, s.filter.as_deref(), "tester", None)
        .await
        .expect("REAL-cursor backfill runs to completion");
    assert!(out.complete, "backfill completed");
    assert_eq!(out.batches, 5, "50 rows / 10 per batch = 5 batches (cursor advanced, not reset)");
    assert_eq!(out.rows_updated, 50, "every row touched ONCE (a reset loop would touch more)");

    // Exactly-once: every row val == id + 1. A dropped-cursor re-scan would push some
    // row to id+2 (double-apply) before the `done=0` filter caught up, or leave a row
    // at id (premature complete) — both surface here.
    let mismatches = scalar_i64(&be, "SELECT count(*) FROM rnums WHERE val <> id + 1").await;
    assert_eq!(mismatches, 0, "REAL-cursor backfill is exactly-once");
    let undone = scalar_i64(&be, "SELECT count(*) FROM rnums WHERE done <> 1").await;
    assert_eq!(undone, 0, "every row marked done");
}

#[compio::test]
async fn sqlite_backfill_real_cursor_resumes_exactly_once_after_crash() {
    let _g = serial();
    let p = paths("bf_real_crash");
    let be = backend(&p);
    seed_real_cursor(&be, 50).await;
    let s = real_spec(10);

    // Phase 1 — exactly 2 committed batches (rows 1..20), then "crash". With the bug,
    // last_cursor was written NULL, so the resume below would re-scan from rk>nothing.
    let out1 = be
        .run_backfill_bounded_sqlite(&s, &s.set_clause, s.filter.as_deref(), "tester", Some(2))
        .await
        .expect("bounded REAL run");
    assert_eq!(out1.batches, 2, "exactly 2 batches before the crash");
    assert_eq!(out1.rows_updated, 20);
    assert!(!out1.complete);

    // Phase 2 — resume. MUST resume from the committed REAL cursor (rk≈20.5), NOT
    // restart from the beginning (the dropped-cursor bug would re-touch rows 1..20).
    let out2 = be
        .run_backfill_bounded_sqlite(&s, &s.set_clause, s.filter.as_deref(), "tester", None)
        .await
        .expect("resumed REAL run");
    assert!(out2.resumed, "the re-run resumed from a committed REAL cursor");
    assert_eq!(out2.rows_updated, 30, "only the remaining 30 rows touched (no re-touch)");
    assert!(out2.complete);

    let mismatches = scalar_i64(&be, "SELECT count(*) FROM rnums WHERE val <> id + 1").await;
    assert_eq!(mismatches, 0, "every row incremented EXACTLY once across the crash (REAL cursor)");
}

// ── non-BINARY (NOCASE) collation cursor: collation-consistent resume ────────
// MED (PR6b code-critic): the window is paged with `ORDER BY <cursor> ASC` +
// `<cursor> > ?1`, which honor the cursor COLUMN's declared SQLite collation
// (NOCASE here). But the high-water mark was computed Rust-side with
// `cells.max()` = BINARY ordering. For a NOCASE TEXT cursor whose BINARY-max ≠
// NOCASE-max within a touched window, the Rust BINARY-max is NOT the column's
// collation-max, so the next batch's `cursor > last_cursor` (compared under
// NOCASE) re-includes an already-touched row — a double-apply, violating the
// headline exactly-once. The PG leg maxes in SQL (`max(_bf_key)::text`) under
// the column's collation and is self-consistent; this proves the SQLite leg now
// matches.
//
// Construction: keys 'a' and 'B'. NOCASE order is ['a','B'] (a<B). BINARY order
// is ['B','a'] ('B'=0x42 < 'a'=0x61). With batch_size=2 the first window touches
// BOTH; the collation-max is 'B', but the BINARY-max is 'a'. A BINARY high-water
// 'a' makes the next window `cursor > 'a'` (NOCASE) re-select 'B' (B>a NOCASE) →
// a SECOND increment of row 'B'. The transform is `val = val + 1` with NO filter
// so a double-apply is directly visible (val lands at the base + 2).

/// Seed `ci(k TEXT COLLATE NOCASE NOT NULL UNIQUE, val INTEGER NOT NULL)` with the
/// two adversarial keys 'a','B' (val=0). The NOCASE collation on the cursor column
/// is the crux: paging honors it but Rust `cells.max()` does not.
async fn seed_nocase_cursor(be: &SqliteBackend) {
    let actor = be.actor();
    actor.set_mode(Mode::CreatorUp).await.unwrap();
    actor
        .exec(
            "CREATE TABLE ci (\
               k   TEXT COLLATE NOCASE NOT NULL UNIQUE, \
               val INTEGER NOT NULL)",
        )
        .await
        .expect("create ci");
    // 'a' (0x61) and 'B' (0x42): NOCASE order a<B, BINARY order B<a.
    actor
        .exec("INSERT INTO ci (k, val) VALUES ('a', 0), ('B', 0)")
        .await
        .expect("seed ci");
}

#[compio::test]
async fn sqlite_backfill_nocase_cursor_exactly_once() {
    let _g = serial();
    let p = paths("bf_nocase");
    let be = backend(&p);
    seed_nocase_cursor(&be).await;

    // batch_size=2 so the first window touches BOTH 'a' and 'B'; NO filter so a
    // re-applied increment is visible. `val = val + 1` is non-idempotent.
    let s = BackfillSpec {
        schema: "main".to_string(),
        table: "ci".to_string(),
        cursor_column: "k".to_string(),
        batch_size: 2,
        set_clause: "\"val\" = (\"val\" + 1)".to_string(),
        filter: None,
        name: "inc_nocase".to_string(),
    };

    let out = be
        .run_backfill_bounded_sqlite(&s, &s.set_clause, s.filter.as_deref(), "tester", None)
        .await
        .expect("nocase backfill runs");
    assert!(out.complete, "backfill completed");
    // Every row touched EXACTLY once: val == 1. A collation-blind BINARY max
    // re-includes 'B' on the next window, landing it at val=2.
    let two = scalar_i64(&be, "SELECT count(*) FROM ci WHERE val = 2").await;
    assert_eq!(two, 0, "no row double-applied (collation-consistent resume cursor)");
    let ones = scalar_i64(&be, "SELECT count(*) FROM ci WHERE val = 1").await;
    assert_eq!(ones, 2, "every row incremented exactly once under a NOCASE cursor");
    assert_eq!(out.rows_updated, 2, "exactly 2 row-updates total (no re-touch)");
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
