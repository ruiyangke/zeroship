//! FAITHFUL e2e for the `SQLite` **batched / resumable backfill executor**
//! against REAL temp-file `SQLite` (no shim, no PG-gating). Drives the real
//! hardened migration connection + the real batched executor, then probes the rows
//! to prove the data transform actually happened, crash-safely and exactly-once.
//!
//! Coverage (the `SQLite` backfill obligations):
//! - a large table transforms in bounded batches, resumable;
//! - crash mid-run (bounded run, no completion) → resume reaches the SAME final
//!   state, EXACTLY ONCE (a `val = val + 1` transform makes a double-apply visible);
//! - the cursor advances in NATURAL (numeric) order, never lexical;
//! - a re-run after completion is an idempotent no-op;
//! - approval/cursor-safety gates fire (defense-in-depth).

// Every test holds the `serial()` `MutexGuard` across its `.await`s ON PURPOSE:
// the guard is a test-only serialization lock over `()` (see `serial`). There is
// no cross-task contention to deadlock (compio test, single executor), so
// `await_holding_lock` is a false positive for this deliberate pattern - allowed
// narrowly, scoped to this one test file.
#![allow(clippy::await_holding_lock)]

use std::path::PathBuf;

use tempfile::TempDir;
use zero_migrate::apply::backend::sqlite::Mode;
use zero_migrate::SqliteBackend;
use zero_migrate::{apply::backend::BackfillError, BackfillSpec, CursorStability};

/// A lock serializing every backfill test in this file.
///
/// It is NOT what keeps an armed fault out of another test's backfill.
/// `zero_migrate::fault::arm` writes a `thread_local!` registry, so a fault armed
/// by one test is scoped to the thread that armed it and cannot fire on another
/// thread - `armed_fault_does_not_cross_thread_boundary` below pins that through
/// the same `BACKFILL_MID_BATCHES` point the crash-fuzz test uses. The one
/// process-global piece, `fault::ARMED_THREADS`, is a counter that gates the
/// fast path; it can only suppress a fire, never cause one.
///
/// That counter is what the lock is for. Three tests here assert on
/// `fault::armed_thread_count()` in absolute terms - nothing armed at entry, one
/// claim after arming, none left after release - and those readings are only true
/// while no other thread in the process holds a claim. The crash-fuzz test arms on
/// its own thread, so an overlapping run makes the counter read two and the
/// assertion fails on a process-wide observation rather than on anything about the
/// backfill under test.
///
/// Measured, not assumed: removing the acquisition from all 18 tests fails
/// `armed_fault_fires_when_armed_on_the_applying_thread` and
/// `armed_fault_claim_is_released_when_a_thread_exits_without_disarming` on every
/// run, with `left: 2, right: 1` and `left: 2, right: 0`. The other 16 pass
/// without it, so the serialization buys them nothing measurable - narrowing the
/// lock to the three counter-observing tests, or having them assert a delta rather
/// than an absolute, would let the rest run in parallel.
fn serial() -> std::sync::MutexGuard<'static, ()> {
    use std::sync::{Mutex, OnceLock};
    static LOCK: OnceLock<Mutex<()>> = OnceLock::new();
    // Recover from a poisoned lock (a panicking test still releases the window).
    LOCK.get_or_init(|| Mutex::new(()))
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
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
    Paths {
        _dir: dir,
        app,
        journal,
    }
}

fn backend(p: &Paths) -> SqliteBackend {
    SqliteBackend::open(&p.app, &p.journal).expect("open hardened sqlite backend")
}

/// Seed a `nums(id INTEGER PRIMARY KEY, val INTEGER, done INTEGER)` table with
/// `n` rows (val = id, done = 0) directly via the actor's `CreatorUp` mode (the
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
        .exec(&format!(
            "INSERT INTO nums (id, val, done) VALUES {}",
            vals.join(", ")
        ))
        .await
        .expect("seed rows");
}

/// Read a single integer scalar via a `CreatorUp` query.
async fn scalar_i64(be: &SqliteBackend, sql: &str) -> i64 {
    let actor = be.actor();
    actor.set_mode(Mode::CreatorUp).await.unwrap();
    let rows = actor.query(sql).await.expect("query");
    rows.first()
        .and_then(|r| r.first())
        .and_then(std::clone::Clone::clone)
        .and_then(|s| s.parse().ok())
        .unwrap_or(-1)
}

fn spec(batch: u32) -> BackfillSpec {
    BackfillSpec {
        // SQLite has one `main` db, no schema namespace; the executor renders the
        // table unqualified. The field is carried for `backfill_id` discrimination.
        schema: "main".to_string(),
        table: "nums".to_string(),
        cursor_columns: vec!["id".to_string()],
        cursor_stability: CursorStability::ExternalInvariant {
            name: "nums_id_is_immutable".to_string(),
        },
        cursor_contract: None,
        batch_size: batch,
        // val = val + 1: a NON-idempotent transform — a double-apply is VISIBLE
        // (a row would land at val = id + 2). Filter to rows not yet done so the
        // exactly-once property is checkable.
        set_clause: "\"val\" = (\"val\" + 1), \"done\" = 1".to_string(),
        per_row: Default::default(),
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

#[compio::test]
async fn sqlite_backfill_rejects_target_triggers_before_mutating_rows() {
    let _g = serial();
    let p = paths("bf_trigger");
    let be = backend(&p);
    seed_nums(&be, 3).await;

    let actor = be.actor();
    actor.set_mode(Mode::CreatorUp).await.unwrap();
    actor
        .exec(
            "CREATE TRIGGER skip_second_row \
             BEFORE UPDATE ON nums \
             WHEN OLD.id = 2 \
             BEGIN SELECT RAISE(IGNORE); END",
        )
        .await
        .expect("create adversarial trigger");

    let s = spec(2);
    let error = be
        .run_backfill_bounded_sqlite(&s, &s.set_clause, s.filter.as_deref(), "tester", None)
        .await
        .expect_err("a target trigger can suppress rows and must be rejected");
    assert!(
        error.to_string().contains("trigger"),
        "the error should explain why the target is unsafe: {error}"
    );
    assert_eq!(
        scalar_i64(&be, "SELECT count(*) FROM nums WHERE done <> 0").await,
        0,
        "trigger rejection must happen before any application row changes"
    );
}

// A selected row must never be skipped silently by a constraint policy.

#[compio::test]
async fn sqlite_backfill_rolls_back_when_conflict_ignore_suppresses_a_selected_row() {
    let _g = serial();
    let p = paths("bf_conflict_ignore");
    let be = backend(&p);

    let actor = be.actor();
    actor.set_mode(Mode::CreatorUp).await.unwrap();
    actor
        .exec(
            "CREATE TABLE ignored_updates (\
                id INTEGER PRIMARY KEY, \
                unique_value INTEGER NOT NULL UNIQUE ON CONFLICT IGNORE, \
                done INTEGER NOT NULL DEFAULT 0\
             ); \
             INSERT INTO ignored_updates (id, unique_value) VALUES (1, 1), (2, 2), (3, 3)",
        )
        .await
        .expect("seed conflict-ignore target");

    let s = BackfillSpec {
        schema: "main".to_string(),
        table: "ignored_updates".to_string(),
        cursor_columns: vec!["id".to_string()],
        cursor_stability: CursorStability::ExternalInvariant {
            name: "ignored_updates_id_is_immutable".to_string(),
        },
        cursor_contract: None,
        batch_size: 2,
        set_clause: "\"unique_value\" = 0, \"done\" = 1".to_string(),
        per_row: Default::default(),
        filter: Some("\"done\" = 0".to_string()),
        name: "conflict_ignore_must_not_skip_rows".to_string(),
    };
    let error = be
        .run_backfill_bounded_sqlite(&s, &s.set_clause, s.filter.as_deref(), "tester", None)
        .await
        .expect_err("a silently ignored update must fail the batch");
    assert!(
        error.to_string().contains("selected 2 rows but updated 1"),
        "the error should report the exact window mismatch: {error}"
    );
    assert_eq!(
        scalar_i64(
            &be,
            "SELECT count(*) FROM ignored_updates WHERE done <> 0 OR unique_value <> id"
        )
        .await,
        0,
        "the mismatched batch must roll back every application row change"
    );
    actor.set_mode(Mode::EngineJournal).await.unwrap();
    let progress = actor
        .query(
            "SELECT last_cursor, rows_done, batches_done, complete \
             FROM \"_mig\".schema_backfills",
        )
        .await
        .expect("inspect journal progress");
    assert_eq!(progress.len(), 1, "the initialized progress row remains");
    assert_eq!(
        progress[0],
        vec![
            None,
            Some("0".to_string()),
            Some("0".to_string()),
            Some("0".to_string()),
        ],
        "the mismatched batch must not checkpoint or complete"
    );
}

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
    assert_eq!(
        out2.rows_updated, 700,
        "only the remaining 700 rows touched"
    );
    assert!(out2.complete);

    // The exactly-once proof: EVERY row is val = id + 1 (none twice, none missed).
    let mismatches = scalar_i64(&be, "SELECT count(*) FROM nums WHERE val <> id + 1").await;
    assert_eq!(
        mismatches, 0,
        "every row incremented EXACTLY once across the crash"
    );
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
    let mut s = spec(100);
    s.cursor_stability = CursorStability::GuardUpdates;

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

    // The zero-migrate guard is a durable obligation: a process interruption does
    // not reopen the cursor-update race before resume.
    let actor = be.actor();
    actor.set_mode(Mode::CreatorUp).await.unwrap();
    let guard_error = actor
        .exec("UPDATE nums SET id = 10000 WHERE id = 250")
        .await
        .expect_err("the cursor guard survives the interrupted apply");
    assert!(
        guard_error.to_string().contains("cursor stability guard"),
        "{guard_error}"
    );

    // Resume — converges to the same final state, exactly once.
    let resumed = be
        .run_backfill_bounded_sqlite(&s, &s.set_clause, s.filter.as_deref(), "tester", None)
        .await
        .expect("resume after fault crash");
    assert!(resumed.resumed, "resumed from the committed cursor");
    assert_eq!(resumed.rows_updated, 300, "only the remaining 300 rows");
    assert!(resumed.complete);
    let mismatches = scalar_i64(&be, "SELECT count(*) FROM nums WHERE val <> id + 1").await;
    assert_eq!(
        mismatches, 0,
        "every row incremented EXACTLY once across the fault crash"
    );
    actor.set_mode(Mode::EngineJournal).await.unwrap();
    let triggers = actor
        .query("SELECT count(*) FROM main.sqlite_schema WHERE type = 'trigger'")
        .await
        .expect("inspect guard cleanup");
    assert_eq!(
        triggers[0][0].as_deref(),
        Some("0"),
        "durable completion cleans up the zero-migrate guard"
    );
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
    assert_eq!(
        mismatches, 0,
        "numeric-order paging touched every row exactly once"
    );
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
    assert_eq!(
        again.batches, 0,
        "completed backfill re-run runs no batches"
    );
    assert_eq!(again.rows_updated, 0);
    let mismatches = scalar_i64(&be, "SELECT count(*) FROM nums WHERE val <> id + 1").await;
    assert_eq!(mismatches, 0, "no double-apply on re-run");
}

// ── fail-closed cursor domains ───────────────────────────────────────────────
// SQLite values are dynamically typed. Durable checkpoints therefore accept only
// the table's single-column INTEGER/TEXT primary key with matching live storage
// classes. Exact UNIQUE candidates are supported, but REAL cannot be round-tripped
// through the tagged cursor checkpoint codec without ordering ambiguity.

/// Seed a table whose requested cursor is REAL and UNIQUE. This isolates the
/// unsupported scalar-domain gate while proving UNIQUE candidates reach it.
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
        // rk = i + 0.5: distinct but not a supported durable cursor domain.
        vals.push(format!("({i}.5, {i}, {i}, 0)"));
    }
    actor
        .exec(&format!(
            "INSERT INTO rnums (rk, id, val, done) VALUES {}",
            vals.join(", ")
        ))
        .await
        .expect("seed rnums");
}

fn real_spec(batch: u32) -> BackfillSpec {
    BackfillSpec {
        schema: "main".to_string(),
        table: "rnums".to_string(),
        cursor_columns: vec!["rk".to_string()],
        cursor_stability: CursorStability::ExternalInvariant {
            name: "rnums_rk_is_immutable".to_string(),
        },
        cursor_contract: None,
        batch_size: batch,
        set_clause: "\"val\" = (\"val\" + 1), \"done\" = 1".to_string(),
        per_row: Default::default(),
        filter: Some("\"done\" = 0".to_string()),
        name: "increment_real".to_string(),
    }
}

#[compio::test]
async fn sqlite_backfill_rejects_unsupported_real_unique_cursor() {
    let _g = serial();
    let p = paths("bf_real");
    let be = backend(&p);
    seed_real_cursor(&be, 50).await;
    let s = real_spec(10);

    let error = be
        .run_backfill_bounded_sqlite(&s, &s.set_clause, s.filter.as_deref(), "tester", None)
        .await
        .expect_err("REAL unique cursors have no supported tagged scalar codec");
    assert!(
        matches!(error, BackfillError::CursorTupleUnavailable { .. }),
        "{error:?}"
    );
    assert_eq!(
        scalar_i64(&be, "SELECT count(*) FROM rnums WHERE done <> 0").await,
        0,
        "rejection happens before data mutation"
    );
}

#[compio::test]
async fn sqlite_backfill_rejects_real_primary_key_cursor() {
    let _g = serial();
    let p = paths("bf_real_crash");
    let be = backend(&p);
    let actor = be.actor();
    actor.set_mode(Mode::CreatorUp).await.unwrap();
    actor
        .exec(
            "CREATE TABLE rnums (\
               rk REAL PRIMARY KEY NOT NULL, \
               val INTEGER NOT NULL, \
               done INTEGER NOT NULL DEFAULT 0); \
             INSERT INTO rnums (rk, val, done) VALUES (1.5, 1, 0), (2.5, 2, 0)",
        )
        .await
        .expect("seed REAL primary key");
    let s = real_spec(10);

    let error = be
        .run_backfill_bounded_sqlite(&s, &s.set_clause, s.filter.as_deref(), "tester", None)
        .await
        .expect_err("REAL primary-key cursors are unsupported");
    assert!(
        matches!(error, BackfillError::CursorTupleUnavailable { .. }),
        "{error:?}"
    );
    assert_eq!(
        scalar_i64(&be, "SELECT count(*) FROM rnums WHERE done <> 0").await,
        0,
        "REAL rejection happens before data mutation"
    );
}

#[compio::test]
async fn sqlite_backfill_rejects_non_utf8_text_keys_before_mutation() {
    let _g = serial();
    let p = paths("bf_non_utf8_text");
    let be = backend(&p);
    let actor = be.actor();
    actor.set_mode(Mode::CreatorUp).await.unwrap();
    actor
        .exec(
            "CREATE TABLE text_keys (k TEXT PRIMARY KEY NOT NULL, done INTEGER NOT NULL); \
             INSERT INTO text_keys (k, done) VALUES \
               (CAST(X'61' AS TEXT), 0), \
               (CAST(X'C0' AS TEXT), 0), \
               (CAST(X'F48FBFBF' AS TEXT), 0)",
        )
        .await
        .expect("seed one invalid UTF-8 key between valid keys");
    let s = BackfillSpec {
        schema: "main".into(),
        table: "text_keys".into(),
        cursor_columns: vec!["k".into()],
        cursor_stability: CursorStability::ExternalInvariant {
            name: "text_keys_k_is_immutable".to_string(),
        },
        cursor_contract: None,
        batch_size: 1,
        set_clause: "\"done\" = 1".into(),
        per_row: Default::default(),
        filter: Some("\"done\" = 0".into()),
        name: "invalid_utf8".into(),
    };

    let error = be
        .run_backfill_bounded_sqlite(&s, &s.set_clause, s.filter.as_deref(), "tester", None)
        .await
        .expect_err("a durable text cursor must have a lossless UTF-8 checkpoint");
    assert!(error.to_string().contains("UTF-8"), "{error}");
    assert_eq!(
        scalar_i64(&be, "SELECT count(*) FROM text_keys WHERE done <> 0").await,
        0,
        "the full cursor domain is checked before the first batch"
    );
}

#[compio::test]
async fn sqlite_backfill_text_key_with_nul_is_checkpointed_with_binds() {
    let _g = serial();
    let p = paths("bf_nul_text");
    let be = backend(&p);
    let actor = be.actor();
    actor.set_mode(Mode::CreatorUp).await.unwrap();
    actor
        .exec(
            "CREATE TABLE text_keys (k TEXT PRIMARY KEY NOT NULL, done INTEGER NOT NULL); \
             INSERT INTO text_keys (k, done) VALUES \
               ('a', 0), (CAST(X'6100' AS TEXT), 0), ('b', 0)",
        )
        .await
        .expect("seed a text key containing NUL");
    let s = BackfillSpec {
        schema: "main".into(),
        table: "text_keys".into(),
        cursor_columns: vec!["k".into()],
        cursor_stability: CursorStability::ExternalInvariant {
            name: "text_keys_k_is_immutable".to_string(),
        },
        cursor_contract: None,
        batch_size: 1,
        set_clause: "\"done\" = 1".into(),
        per_row: Default::default(),
        filter: Some("\"done\" = 0".into()),
        name: "nul_text".into(),
    };

    let outcome = be
        .run_backfill_bounded_sqlite(&s, &s.set_clause, s.filter.as_deref(), "tester", None)
        .await
        .expect("NUL is data, not SQL syntax, when checkpoints use native binds");
    assert!(outcome.complete);
    assert_eq!(outcome.rows_updated, 3);
    assert_eq!(
        scalar_i64(&be, "SELECT count(*) FROM text_keys WHERE done = 1").await,
        3
    );
}

// ── non-BINARY (NOCASE) collation cursor: collation-consistent resume ────────
// The window is paged with `ORDER BY <cursor> ASC` +
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

/// Seed `ci(k TEXT COLLATE NOCASE PRIMARY KEY NOT NULL, val INTEGER NOT NULL)` with the
/// two adversarial keys 'a','B' (val=0). The NOCASE collation on the cursor column
/// is the crux: paging honors it but Rust `cells.max()` does not.
async fn seed_nocase_cursor(be: &SqliteBackend) {
    let actor = be.actor();
    actor.set_mode(Mode::CreatorUp).await.unwrap();
    actor
        .exec(
            "CREATE TABLE ci (\
               k   TEXT COLLATE NOCASE PRIMARY KEY NOT NULL, \
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
        cursor_columns: vec!["k".to_string()],
        cursor_stability: CursorStability::ExternalInvariant {
            name: "ci_k_is_immutable".to_string(),
        },
        cursor_contract: None,
        batch_size: 2,
        set_clause: "\"val\" = (\"val\" + 1)".to_string(),
        per_row: Default::default(),
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
    assert_eq!(
        two, 0,
        "no row double-applied (collation-consistent resume cursor)"
    );
    let ones = scalar_i64(&be, "SELECT count(*) FROM ci WHERE val = 1").await;
    assert_eq!(
        ones, 2,
        "every row incremented exactly once under a NOCASE cursor"
    );
    assert_eq!(
        out.rows_updated, 2,
        "exactly 2 row-updates total (no re-touch)"
    );
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
    s.cursor_columns = vec!["val".to_string()];
    s.set_clause = "\"done\" = 1".to_string();
    let err = be
        .run_backfill_bounded_sqlite(&s, &s.set_clause, s.filter.as_deref(), "tester", None)
        .await
        .unwrap_err();
    assert!(
        matches!(err, BackfillError::CursorTupleUnavailable { .. }),
        "{err:?}"
    );
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
    assert!(
        matches!(err, BackfillError::CursorComponentMutated { .. }),
        "{err:?}"
    );
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
    assert!(
        matches!(err, BackfillError::InvalidIdentifier { what: "table", .. }),
        "{err:?}"
    );
}

// ---- the armed-fault registry is per-thread ---------------------------------
// `fault::arm` writes a `thread_local!`, so a fault armed on one thread must not
// fire on another. That per-thread scoping is the ONLY reason the ungated
// `fault` module is unreachable from the shipping Node consumer, which runs every
// apply on a freshly spawned worker thread (`run_engine_blocking` in
// `zero-migrate-node/src/runtime.rs`) that never arms anything. These tests pin
// the boundary through the same surface the crash-fuzz suite uses: a real SQLite
// backfill over the real BACKFILL_MID_BATCHES trip point.
//
// What this does NOT cover: it does not make the seam safe for a Rust embedder
// that arms and applies on ONE thread. The positive control below is exactly that
// case and it fires. It also does not cover the PG/MySQL trip points, which are
// the same `fault::trip` call and the same registry.

/// Seed 500 rows into a fresh temp `SQLite` pair and run one unbounded backfill
/// to completion on the CURRENT thread, driving the future with a reactor-less
/// `futures::executor::block_on` (the shape the node worker thread uses).
/// `arm_here` first arms the mid-batches fault ON THIS THREAD, to fire after the
/// 2nd committed batch.
fn backfill_on_this_thread(
    tag: &str,
    arm_here: bool,
) -> Result<zero_migrate::apply::backend::BackfillOutcome, BackfillError> {
    use zero_migrate::fault;

    futures::executor::block_on(async {
        let p = paths(tag);
        let be = backend(&p);
        seed_nums(&be, 500).await;
        if arm_here {
            fault::arm(fault::points::BACKFILL_MID_BATCHES, 1);
        }
        let s = spec(100);
        be.run_backfill_bounded_sqlite(&s, &s.set_clause, s.filter.as_deref(), "tester", None)
            .await
    })
}

#[test]
fn armed_fault_does_not_cross_thread_boundary() {
    use zero_migrate::fault;

    let _g = serial();
    fault::disarm_all();

    // Thread A (this one) arms the fault the backfill loop trips once per
    // committed batch, and never runs a backfill itself.
    fault::arm(fault::points::BACKFILL_MID_BATCHES, 1);

    // Assert the arming REACHED the registry before asserting a fault does not
    // cross to thread B. Without this the test says only "nothing fired", which is
    // equally true when `arm` does nothing at all - the assertions below about the
    // completed apply are about the backfill, not about the fault. This line is what
    // makes the test fail on a gutted `arm` rather than leaning on the positive
    // control in the next test to supply its meaning.
    assert_eq!(
        fault::armed_thread_count(),
        1,
        "thread A holds an armed fault before thread B applies"
    );

    // Thread B runs the whole apply. It trips the armed point 5 times (500 rows /
    // 100 per batch) and must never see thread A's fault.
    let out = std::thread::spawn(|| backfill_on_this_thread("bf_xthread_neg", false))
        .join()
        .expect("worker thread did not panic")
        .expect("a fault armed on another thread must not abort this apply");
    assert!(out.complete, "the apply completed");
    assert_eq!(out.batches, 5, "all 5 batches ran; none was cut short");
    assert_eq!(out.rows_updated, 500, "every row touched");

    // Thread A's fault is still armed and still unfired: `disarm_all` clears the
    // CALLING thread's registry, so this is the call that releases it.
    fault::disarm_all();
    assert_eq!(
        fault::armed_thread_count(),
        0,
        "disarm_all released this thread's claim"
    );
}

// The positive control for the test above. Without it a never-firing fault would
// pass the negative test against any implementation, including one where `arm` is
// a no-op.
#[test]
fn armed_fault_fires_when_armed_on_the_applying_thread() {
    use zero_migrate::fault;

    let _g = serial();
    fault::disarm_all();

    // Same worker-thread shape as the negative test, except the worker arms the
    // fault ITSELF before applying: the Rust-embedder case, where the seam is live.
    let out = std::thread::spawn(|| backfill_on_this_thread("bf_xthread_pos", true))
        .join()
        .expect("worker thread did not panic");
    let err = out.expect_err("a fault armed on the applying thread aborts the run");
    assert!(matches!(err, BackfillError::Fault(_)), "{err:?}");

    // The fault fired, which self-disarms and releases the worker's claim.
    assert_eq!(
        fault::armed_thread_count(),
        0,
        "a fired one-shot fault releases the arming thread's claim"
    );
}

// A thread that arms a fault, never trips it, and exits without calling
// `disarm_all` must not leak its ARMED_THREADS claim: a leaked claim permanently
// disables `trip`'s single-atomic-load fast path, so every trip point in the
// process would pay a TLS access plus a HashMap lookup for the rest of the run.
// The `Registry` Drop guard releases it at thread exit. This does NOT cover a
// thread killed without running destructors (process::exit/abort).
#[test]
fn armed_fault_claim_is_released_when_a_thread_exits_without_disarming() {
    use zero_migrate::fault;

    let _g = serial();
    fault::disarm_all();
    assert_eq!(
        fault::armed_thread_count(),
        0,
        "no other test in this binary left a fault armed"
    );

    std::thread::spawn(|| {
        fault::arm(fault::points::BACKFILL_MID_BATCHES, 1_000_000);
        assert_eq!(
            fault::armed_thread_count(),
            1,
            "arming claimed a slot for this thread"
        );
        // Exit WITHOUT disarming, with the fault still unfired.
    })
    .join()
    .expect("worker thread did not panic");

    assert_eq!(
        fault::armed_thread_count(),
        0,
        "thread exit released the armed-fault claim (no leak)"
    );
}

// ── PROBE: a SUPPORTED affinity holding a MISMATCHED live storage class ──────
//
// `docs/security-model.md`: "A SQLite backfill additionally requires an exact
// ordered, non-null primary or unique candidate-key tuple with supported declared
// `INTEGER` or `TEXT` affinity. Every live cursor value must use the matching
// storage class."
//
// Every rejection arm above measures the first sentence - the DECLARED affinity
// (REAL declared, non-UTF8 text). None measures the second, which is a different
// claim and the one SQLite's dynamic typing makes possible: a column DECLARED
// `INTEGER` accepts a TEXT value whenever the text will not convert losslessly,
// so `k INTEGER` can hold `'abc'` and still satisfy its UNIQUE index.
//
// That matters for a cursor because SQLite orders ACROSS storage classes
// (NULL < INTEGER/REAL < TEXT < BLOB), so a mixed-class key column orders by class
// first. A resume that bound a saved INTEGER cursor and asked for `k > ?` would
// step over every TEXT-classed row silently - they sort after all integers - and
// the run would report success having skipped them.
#[compio::test]
async fn sqlite_backfill_rejects_a_text_value_living_in_an_integer_cursor() {
    let _g = serial();
    let p = paths("bf_mixed_class");
    let be = backend(&p);
    let actor = be.actor();
    actor.set_mode(Mode::CreatorUp).await.unwrap();
    // `k` is INTEGER-affinity and UNIQUE but NOT the rowid alias, so SQLite stores
    // a non-convertible text value as TEXT rather than coercing it.
    actor
        .exec(
            "CREATE TABLE mixed (\
               k INTEGER NOT NULL UNIQUE, \
               val INTEGER NOT NULL, \
               done INTEGER NOT NULL DEFAULT 0); \
             INSERT INTO mixed (k, val, done) VALUES (1, 1, 0), (2, 2, 0), ('abc', 3, 0)",
        )
        .await
        .expect("seed a mixed-storage-class cursor column");

    // The setup control: SQLite really did keep one value as TEXT. Without this the
    // arm could pass on a table where everything coerced to INTEGER, which is a
    // different (and safe) shape entirely.
    assert_eq!(
        scalar_i64(&be, "SELECT count(*) FROM mixed WHERE typeof(k) = 'text'").await,
        1,
        "the fixture needs one genuinely TEXT-classed value in the INTEGER column"
    );

    let spec = BackfillSpec {
        schema: "main".to_string(),
        table: "mixed".to_string(),
        cursor_columns: vec!["k".to_string()],
        cursor_stability: CursorStability::ExternalInvariant {
            name: "mixed_k_is_immutable".to_string(),
        },
        cursor_contract: None,
        batch_size: 1,
        set_clause: "\"val\" = (\"val\" + 1), \"done\" = 1".to_string(),
        per_row: Default::default(),
        filter: Some("\"done\" = 0".to_string()),
        name: "increment_mixed".to_string(),
    };

    let error = be
        .run_backfill_bounded_sqlite(
            &spec,
            &spec.set_clause,
            spec.filter.as_deref(),
            "tester",
            None,
        )
        .await
        .expect_err("a mixed-storage-class cursor column must be refused");
    match &error {
        BackfillError::CursorTupleUnavailable { reason, .. } => assert!(
            reason.contains("runtime storage class") && reason.contains("integer"),
            "the refusal must name the RUNTIME storage class rather than the declared \
             affinity, got {reason:?}"
        ),
        other => panic!("expected CursorTupleUnavailable, got {other:?}"),
    }

    // Refused BEFORE mutation, which is what makes the rule safe rather than
    // merely noisy: the two well-typed rows were reachable and still untouched.
    assert_eq!(
        scalar_i64(&be, "SELECT count(*) FROM mixed WHERE done <> 0").await,
        0,
        "the storage-class rejection must land before any row is written"
    );
}

/// The control for the arm above: the SAME shape with every value well-typed
/// still backfills.
///
/// Without it, "mixed classes are refused" also holds for a build that refused
/// every non-rowid INTEGER cursor, and the arm above would be reporting a working
/// rule over a dead one.
#[compio::test]
async fn sqlite_backfill_accepts_a_non_rowid_integer_cursor_when_every_value_matches() {
    let _g = serial();
    let p = paths("bf_uniform_class");
    let be = backend(&p);
    let actor = be.actor();
    actor.set_mode(Mode::CreatorUp).await.unwrap();
    actor
        .exec(
            "CREATE TABLE uniform (\
               k INTEGER NOT NULL UNIQUE, \
               val INTEGER NOT NULL, \
               done INTEGER NOT NULL DEFAULT 0); \
             INSERT INTO uniform (k, val, done) VALUES (1, 1, 0), (2, 2, 0), (3, 3, 0)",
        )
        .await
        .expect("seed a uniformly INTEGER-classed cursor column");

    let spec = BackfillSpec {
        schema: "main".to_string(),
        table: "uniform".to_string(),
        cursor_columns: vec!["k".to_string()],
        cursor_stability: CursorStability::ExternalInvariant {
            name: "uniform_k_is_immutable".to_string(),
        },
        cursor_contract: None,
        batch_size: 1,
        set_clause: "\"val\" = (\"val\" + 1), \"done\" = 1".to_string(),
        per_row: Default::default(),
        filter: Some("\"done\" = 0".to_string()),
        name: "increment_uniform".to_string(),
    };

    let out = be
        .run_backfill_bounded_sqlite(
            &spec,
            &spec.set_clause,
            spec.filter.as_deref(),
            "tester",
            None,
        )
        .await
        .expect("a uniformly typed non-rowid INTEGER cursor is supported");
    assert!(out.complete, "the run completed");
    assert_eq!(out.rows_updated, 3, "every row was backfilled");
    assert_eq!(
        scalar_i64(&be, "SELECT count(*) FROM uniform WHERE val <> k + 1").await,
        0,
        "every row incremented exactly once"
    );
}
