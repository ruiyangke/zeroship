//! A driver-neutral **conformance suite** for the [`SqlSession`](super::SqlSession)
//! seam.
//!
//! Every host driver (the napi `pg`/`mysql2` shells, the in-crate `PgDevSession`
//! test driver) claims to honour a small set of seam invariants the engine's apply
//! path RELIES on but never re-checks:
//!
//! 1. **Session pinning.** The engine issues its whole apply as a strictly
//!    one-verb-at-a-time sequence over ONE pinned connection. A temp object created
//!    by one verb (a `TEMP TABLE`, a `SET`, an open transaction) MUST be visible to
//!    the next verb. A driver that silently round-robins a pool would pass the
//!    RecordingSession smoke test yet corrupt a real apply (the `BEGIN` lands on one
//!    backend, the `COMMIT` on another). This suite proves the driver holds one
//!    backend across verbs.
//! 2. **Transaction visibility.** `batch("BEGIN")` → `exec(INSERT …)` →
//!    `query(SELECT …)` sees the row *inside* the txn; a subsequent
//!    `batch("ROLLBACK")` discards it. This is the exact discipline
//!    `apply_transactional` depends on.
//! 3. **`exec` vs `exec_text` param semantics.** `exec` binds neutral [`Bind`]s;
//!    `exec_text` sends every param as server-inferred TEXT (the load-bearing
//!    `text → timestamptz` coercion path, `executor.rs`'s `apply_dml_transactional`).
//!    A `None` text param is a SQL NULL. This suite proves both param sides.
//! 4. **Error + SQLSTATE mapping.** A failing statement surfaces a [`DbError`] whose
//!    `message` is non-empty and whose `sqlstate` (when the driver has one) is the
//!    real Postgres SQLSTATE — not a stringified panic. Every `#[source]` wrap
//!    reads this.
//!
//! This is the FIRST external consumer of the seam beyond the engine itself: a
//! driver author (or the `PgDevSession` test harness) runs [`run`] against a live,
//! empty session and gets a single pass/fail verdict with a precise reason. It is
//! deliberately **schema-agnostic** — it creates and drops its own scratch objects
//! in a caller-provided scratch schema, touching nothing the engine journals.
//!
//! Postgres-flavoured by design (it issues `BEGIN`/`ROLLBACK`, a `TEMP TABLE`, and a
//! text→timestamptz coercion). A MySQL conformance profile would render the dialect
//! equivalents; the shape (four checks, one verdict) is the same. Gated on
//! `pg_seam` because it names the `SqlSession` seam.

use super::{Bind, DbError, SqlSession};

/// A conformance failure: which check failed, and why.
#[derive(Debug, Clone)]
pub struct ConformanceFailure {
    /// The check that failed (`"session-pinning"`, `"transaction-visibility"`,
    /// `"exec-text-semantics"`, `"error-sqlstate-mapping"`).
    pub check: &'static str,
    /// A precise, human-readable reason.
    pub reason: String,
}

impl std::fmt::Display for ConformanceFailure {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            f,
            "seam conformance check `{}` failed: {}",
            self.check, self.reason
        )
    }
}

impl std::error::Error for ConformanceFailure {}

fn fail(check: &'static str, reason: impl Into<String>) -> ConformanceFailure {
    ConformanceFailure {
        check,
        reason: reason.into(),
    }
}

/// Run the full [`SqlSession`] conformance suite against a live `session`, using
/// `scratch_table` as a caller-owned temp-table name (must be a bare, unqualified
/// identifier — the check creates it `TEMP` so it never touches a real schema).
///
/// Returns `Ok(())` if the driver honours all four seam invariants, or the FIRST
/// failing [`ConformanceFailure`]. The suite leaves no residue: the temp table is
/// session-scoped and the transaction check rolls itself back.
///
/// # Errors
/// The first [`ConformanceFailure`]; a `DbError` from an unexpected infrastructure
/// failure is wrapped as a failure of the check that provoked it.
pub async fn run<S: SqlSession>(
    session: &S,
    scratch_table: &str,
) -> Result<(), ConformanceFailure> {
    check_session_pinning(session, scratch_table).await?;
    check_transaction_visibility(session, scratch_table).await?;
    check_exec_text_semantics(session).await?;
    check_error_sqlstate_mapping(session).await?;
    Ok(())
}

/// Check 1 — session pinning: a `TEMP TABLE` created by one verb is visible to a
/// later verb on the SAME session. A pooled/round-robin driver fails here (the
/// second verb's backend cannot see the first's temp object).
async fn check_session_pinning<S: SqlSession>(
    session: &S,
    scratch_table: &str,
) -> Result<(), ConformanceFailure> {
    const CHECK: &str = "session-pinning";
    // A TEMP table lives for the session only, on the backend that created it.
    session
        .batch(&format!(
            "CREATE TEMP TABLE {scratch_table} (id int8, note text)"
        ))
        .await
        .map_err(|e| fail(CHECK, format!("could not create scratch TEMP table: {e}")))?;
    session
        .exec(
            &format!("INSERT INTO {scratch_table} (id, note) VALUES ($1, $2)"),
            &[Bind::Int(7), Bind::Text("pinned".to_string())],
        )
        .await
        .map_err(|e| fail(CHECK, format!("INSERT into scratch temp table failed: {e}")))?;
    // The read MUST see the row — proving the same backend serviced all three verbs.
    let rows = session
        .query(
            &format!("SELECT id, note FROM {scratch_table} WHERE id = $1"),
            &[Bind::Int(7)],
        )
        .await
        .map_err(|e| fail(CHECK, format!("SELECT from scratch temp table failed: {e}")))?;
    if rows.len() != 1 {
        return Err(fail(
            CHECK,
            format!(
                "expected exactly 1 row from the pinned TEMP table, got {} — the \
                 driver is not pinning ONE connection across verbs",
                rows.len()
            ),
        ));
    }
    let id: i64 = rows[0]
        .try_get("id")
        .map_err(|e| fail(CHECK, format!("could not decode `id` (int8 → Int): {e}")))?;
    let note: String = rows[0]
        .try_get("note")
        .map_err(|e| fail(CHECK, format!("could not decode `note` (text → Text): {e}")))?;
    if id != 7 || note != "pinned" {
        return Err(fail(
            CHECK,
            format!("round-tripped ({id}, {note:?}), expected (7, \"pinned\")"),
        ));
    }
    // Clean the scratch table so the transaction-visibility check reuses the name.
    session
        .batch(&format!("DROP TABLE {scratch_table}"))
        .await
        .map_err(|e| fail(CHECK, format!("could not drop scratch TEMP table: {e}")))?;
    Ok(())
}

/// Check 2 — transaction visibility: a row written inside an explicit `BEGIN` is
/// visible to a `query` on the same session BEFORE commit, and a `ROLLBACK`
/// discards it. This is the exact `apply_transactional` discipline.
async fn check_transaction_visibility<S: SqlSession>(
    session: &S,
    scratch_table: &str,
) -> Result<(), ConformanceFailure> {
    const CHECK: &str = "transaction-visibility";
    session
        .batch(&format!("CREATE TEMP TABLE {scratch_table} (id int8)"))
        .await
        .map_err(|e| fail(CHECK, format!("create scratch table: {e}")))?;
    session
        .batch("BEGIN")
        .await
        .map_err(|e| fail(CHECK, format!("BEGIN: {e}")))?;
    session
        .exec(
            &format!("INSERT INTO {scratch_table} (id) VALUES ($1)"),
            &[Bind::Int(99)],
        )
        .await
        .map_err(|e| fail(CHECK, format!("in-txn INSERT: {e}")))?;
    // Visible inside the open txn on the SAME session.
    let in_txn = session
        .query_one(
            &format!("SELECT count(*)::int8 AS n FROM {scratch_table}"),
            &[],
        )
        .await
        .map_err(|e| fail(CHECK, format!("in-txn count read: {e}")))?;
    let n_in: i64 = in_txn
        .try_get("n")
        .map_err(|e| fail(CHECK, format!("decode in-txn count (int8 → Int): {e}")))?;
    if n_in != 1 {
        // Roll back before returning so we leave no open txn.
        let _ = session.batch("ROLLBACK").await;
        return Err(fail(
            CHECK,
            format!(
                "in-txn count = {n_in}, expected 1 — the INSERT is not visible \
                     to a same-session read inside the open transaction"
            ),
        ));
    }
    session
        .batch("ROLLBACK")
        .await
        .map_err(|e| fail(CHECK, format!("ROLLBACK: {e}")))?;
    // After rollback the row is gone.
    let after = session
        .query_one(
            &format!("SELECT count(*)::int8 AS n FROM {scratch_table}"),
            &[],
        )
        .await
        .map_err(|e| fail(CHECK, format!("post-rollback count read: {e}")))?;
    let n_after: i64 = after
        .try_get("n")
        .map_err(|e| fail(CHECK, format!("decode post-rollback count: {e}")))?;
    if n_after != 0 {
        return Err(fail(
            CHECK,
            format!(
                "post-ROLLBACK count = {n_after}, expected 0 — ROLLBACK did not \
                     discard the in-txn write"
            ),
        ));
    }
    session
        .batch(&format!("DROP TABLE {scratch_table}"))
        .await
        .map_err(|e| fail(CHECK, format!("drop scratch table: {e}")))?;
    Ok(())
}

/// Check 3 — `exec_text` semantics: a text param with NO explicit OID is coerced by
/// the server to the target column type (the `text → timestamptz` path), and a
/// `None` text param is a SQL NULL. This is the load-bearing distinction between
/// `exec` (typed binds) and `exec_text` (all-text, server-inferred).
async fn check_exec_text_semantics<S: SqlSession>(session: &S) -> Result<(), ConformanceFailure> {
    const CHECK: &str = "exec-text-semantics";
    // A scratch temp table with a timestamptz column: the coercion the engine's
    // op.* DML path relies on (a text `'2026-01-02T03:04:05Z'` → timestamptz).
    session
        .batch("CREATE TEMP TABLE zm_conf_text (id int8, ts timestamptz, tag text)")
        .await
        .map_err(|e| fail(CHECK, format!("create text-coercion scratch table: {e}")))?;
    // exec_text: every param crosses as server-inferred TEXT. `'7'` → int8,
    // `'2026-01-02T03:04:05Z'` → timestamptz, `None` → SQL NULL. A concrete-OID
    // binary bind of a text value against timestamptz would be REFUSED — this path
    // must not be.
    let affected = session
        .exec_text(
            "INSERT INTO zm_conf_text (id, ts, tag) VALUES ($1, $2, $3)",
            &[
                Some("7".to_string()),
                Some("2026-01-02T03:04:05Z".to_string()),
                None,
            ],
        )
        .await
        .map_err(|e| {
            fail(
                CHECK,
                format!("exec_text INSERT with a text→timestamptz coercion failed: {e}"),
            )
        })?;
    if affected != 1 {
        return Err(fail(
            CHECK,
            format!("exec_text reported {affected} rows affected, expected 1"),
        ));
    }
    // Read back: the id coerced to int8, the tag is a genuine SQL NULL.
    let row = session
        .query_one(
            "SELECT id, tag, (ts = timestamptz '2026-01-02T03:04:05Z') AS ts_ok \
             FROM zm_conf_text WHERE id = 7",
            &[],
        )
        .await
        .map_err(|e| fail(CHECK, format!("read-back after exec_text: {e}")))?;
    let id: i64 = row
        .try_get("id")
        .map_err(|e| fail(CHECK, format!("decode coerced id (text→int8): {e}")))?;
    if id != 7 {
        return Err(fail(CHECK, format!("coerced id = {id}, expected 7")));
    }
    // The NULL tag decodes as Option::None (a genuine SQL NULL, not the string "").
    let tag: Option<String> = row
        .try_get("tag")
        .map_err(|e| fail(CHECK, format!("decode NULL tag: {e}")))?;
    if tag.is_some() {
        return Err(fail(
            CHECK,
            format!("None text param did not become SQL NULL (got {tag:?})"),
        ));
    }
    let ts_ok: bool = row
        .try_get("ts_ok")
        .map_err(|e| fail(CHECK, format!("decode ts coercion bool: {e}")))?;
    if !ts_ok {
        return Err(fail(
            CHECK,
            "the text `ts` param did not coerce to the expected timestamptz value",
        ));
    }
    // Also assert `exec` (typed binds) round-trips a NULL Bind on a text column —
    // the exact shape the shipped `exec` path binds a NULL (a nullable text
    // `last_cursor`, `backfill.rs`), never against a timestamptz (that path is
    // `exec_text`). A `Bind::Null` must land as a SQL NULL and read back as
    // `Option::None`.
    let n = session
        .exec(
            "INSERT INTO zm_conf_text (id, ts, tag) VALUES ($1, now(), $2)",
            &[Bind::Int(8), Bind::Null],
        )
        .await
        .map_err(|e| {
            fail(
                CHECK,
                format!("exec with a Bind::Null (text) param failed: {e}"),
            )
        })?;
    if n != 1 {
        return Err(fail(CHECK, format!("exec reported {n} rows, expected 1")));
    }
    let null_row = session
        .query_one("SELECT tag FROM zm_conf_text WHERE id = 8", &[])
        .await
        .map_err(|e| fail(CHECK, format!("read-back Bind::Null row: {e}")))?;
    match null_row.try_get::<_, Option<String>>("tag") {
        Ok(None) => {}
        Ok(Some(v)) => {
            return Err(fail(
                CHECK,
                format!("Bind::Null did not produce a SQL NULL (tag = {v:?})"),
            ))
        }
        Err(e) => return Err(fail(CHECK, format!("decode nullable tag: {e}"))),
    }
    session
        .batch("DROP TABLE zm_conf_text")
        .await
        .map_err(|e| fail(CHECK, format!("drop text-coercion scratch table: {e}")))?;
    Ok(())
}

/// Check 4 — error + SQLSTATE mapping: a statement that fails at the server
/// surfaces a [`DbError`] with a non-empty message and (when the driver carries it)
/// the real Postgres SQLSTATE. A driver that swallows the error, or stringifies a
/// panic, fails here.
async fn check_error_sqlstate_mapping<S: SqlSession>(
    session: &S,
) -> Result<(), ConformanceFailure> {
    const CHECK: &str = "error-sqlstate-mapping";
    // A deliberate undefined-table error (SQLSTATE 42P01). It runs OUTSIDE any txn
    // so it does not poison the session.
    let err: DbError = match session
        .query("SELECT 1 FROM a_table_that_does_not_exist_zm_conf", &[])
        .await
    {
        Ok(_) => {
            return Err(fail(
                CHECK,
                "a query against a non-existent table SUCCEEDED — the driver is \
                 swallowing errors",
            ))
        }
        Err(e) => e,
    };
    if err.message.trim().is_empty() {
        return Err(fail(CHECK, "the DbError carried an empty message"));
    }
    // The SQLSTATE is optional in the seam, but a Postgres driver that surfaces one
    // MUST surface the real code. 42P01 = undefined_table. We accept either "the
    // driver carries no sqlstate" (message-only is a valid seam contract) or "it
    // carries the correct one" — but a WRONG non-empty sqlstate is a bug.
    if let Some(state) = &err.sqlstate {
        if state != "42P01" {
            return Err(fail(
                CHECK,
                format!("expected SQLSTATE 42P01 (undefined_table), got {state:?}"),
            ));
        }
    }
    // Prove the session is still usable after the error (the driver did not wedge
    // the pinned connection): a trivial query still runs and decodes.
    let alive = session
        .query_one("SELECT 1::int8 AS one", &[])
        .await
        .map_err(|e| {
            fail(
                CHECK,
                format!("session unusable after a caught error (connection wedged): {e}"),
            )
        })?;
    match alive.try_get::<_, i64>("one") {
        Ok(1) => {}
        Ok(other) => {
            return Err(fail(
                CHECK,
                format!("post-error liveness probe returned {other}, expected 1"),
            ))
        }
        Err(e) => return Err(fail(CHECK, format!("decode liveness probe: {e}"))),
    }
    Ok(())
}
