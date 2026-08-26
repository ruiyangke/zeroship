//! A driver-neutral **conformance suite** for the
//! [`SqlSession`](crate::driver::SqlSession) seam.
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
//! 2. **Transaction visibility.** `batch("BEGIN")` -> `exec(INSERT ...)` ->
//!    `query(SELECT ...)` sees the row *inside* the txn; a subsequent
//!    `batch("ROLLBACK")` discards it. This is the exact discipline
//!    `apply_transactional` depends on.
//! 3. **Bind-inference semantics.** A DECLARED bind carries its type; a
//!    [`Bind::Inferred`] one carries none and the server types it from context.
//!    That difference is the load-bearing `text` to timestamp coercion path the
//!    DML executor depends on, and `Inferred(None)` is a SQL NULL. This suite
//!    proves both kinds, separately and mixed in one statement.
//! 4. **Error + SQLSTATE mapping.** A failing statement surfaces a [`DbError`] whose
//!    `message` is non-empty and whose `sqlstate` (when the driver has one) is the
//!    real server SQLSTATE - not a stringified panic. Every `#[source]` wrap
//!    reads this.
//!
//! This is the FIRST external consumer of the seam beyond the engine itself: a
//! driver author (or the `PgDevSession` test harness) runs
//! [`crate::driver::conformance::run`] against a live,
//! empty session and gets a single pass/fail verdict with a precise reason. It is
//! deliberately **schema-agnostic** - it creates and drops its own scratch objects
//! in a caller-provided scratch schema, touching nothing the engine journals.
//!
//! The checks are neutral; the scratch SQL that provokes them is not, so the
//! caller passes a [`crate::driver::conformance::SeamFixture`] carrying its own
//! spellings - the session-scoped table keyword, the integer and timestamp
//! types, the placeholder form, an integer cast, and the SQLSTATE for a missing
//! table. PostgreSQL and MySQL each supply one and both run this suite live.

use super::{Bind, DbError, SqlSession};

/// A conformance failure: which check failed, and why.
#[derive(Debug, Clone)]
pub struct ConformanceFailure {
    /// The check that failed (`"session-pinning"`, `"transaction-visibility"`,
    /// `"bind-inference-semantics"`, `"error-sqlstate-mapping"`).
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

/// The handful of dialect spellings the conformance FIXTURE needs.
///
/// The invariants this suite checks are neutral - pinning, transaction
/// visibility, param-format semantics and error surfacing are properties of a
/// DRIVER, not of a grammar. The scratch SQL that provokes them is not: a
/// temp-table declaration, a positional placeholder and an integer cast are all
/// spelled differently per vendor.
///
/// So the caller supplies them. It knows its own dialect; this crate is the
/// neutral contract and must not learn one. That split is also what lets a
/// fourth backend run the same suite by writing one of these rather than
/// forking the checks.
#[derive(Debug, Clone, Copy)]
pub struct SeamFixture {
    /// The session-scoped table modifier: `TEMP` or `TEMPORARY`.
    pub temp_keyword: &'static str,
    /// The 64-bit signed integer column type.
    pub bigint_type: &'static str,
    /// The timestamp column type used by the inference-coercion check.
    pub timestamp_type: &'static str,
    /// The instant an inferred bind sends as TEXT, spelled the way this server
    /// parses it. This is the value under test: the whole reason a bind can be
    /// inferred is that it must arrive as text and be coerced server-side.
    pub timestamp_text_param: &'static str,
    /// A boolean SQL expression asserting column `ts` equals
    /// [`Self::timestamp_text_param`]. Kept a callback because a vendor may need
    /// to type its literal for the comparison to mean anything.
    pub ts_matches: fn() -> String,
    /// Render the 1-based positional placeholder for bind `n`.
    pub placeholder: fn(usize) -> String,
    /// Wrap a scalar expression so the server returns it as a 64-bit integer.
    pub as_bigint: fn(&str) -> String,
    /// The SQLSTATE the server raises for a reference to a missing table. A
    /// driver may carry no SQLSTATE at all, but a driver that carries one MUST
    /// carry this one.
    pub undefined_table_sqlstate: &'static str,
}

/// Run the full [`SqlSession`] conformance suite against a live `session`, using
/// `scratch_table` as a caller-owned temp-table name (must be a bare, unqualified
/// identifier - the check creates it session-scoped so it never touches a real
/// schema) and `fixture` for this dialect's scratch spellings.
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
    fixture: &SeamFixture,
) -> Result<(), ConformanceFailure> {
    check_session_pinning(session, scratch_table, fixture).await?;
    check_transaction_visibility(session, scratch_table, fixture).await?;
    check_bind_inference_semantics(session, fixture).await?;
    check_error_sqlstate_mapping(session, fixture).await?;
    Ok(())
}

/// Check 1 - session pinning: a `TEMP TABLE` created by one verb is visible to a
/// later verb on the SAME session. A pooled/round-robin driver fails here (the
/// second verb's backend cannot see the first's temp object).
async fn check_session_pinning<S: SqlSession>(
    session: &S,
    scratch_table: &str,
    fixture: &SeamFixture,
) -> Result<(), ConformanceFailure> {
    const CHECK: &str = "session-pinning";
    let temp = fixture.temp_keyword;
    let int8 = fixture.bigint_type;
    let p1 = (fixture.placeholder)(1);
    let p2 = (fixture.placeholder)(2);
    // A TEMP table lives for the session only, on the backend that created it.
    session
        .batch(&format!(
            "CREATE {temp} TABLE {scratch_table} (id {int8}, note text)"
        ))
        .await
        .map_err(|e| fail(CHECK, format!("could not create scratch TEMP table: {e}")))?;
    session
        .exec(
            &format!("INSERT INTO {scratch_table} (id, note) VALUES ({p1}, {p2})"),
            &[Bind::Int(7), Bind::Text("pinned".to_string())],
        )
        .await
        .map_err(|e| fail(CHECK, format!("INSERT into scratch temp table failed: {e}")))?;
    // The read MUST see the row - proving the same backend serviced all three verbs.
    let rows = session
        .query(
            &format!("SELECT id, note FROM {scratch_table} WHERE id = {p1}"),
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

/// Check 2 - transaction visibility: a row written inside an explicit `BEGIN` is
/// visible to a `query` on the same session BEFORE commit, and a `ROLLBACK`
/// discards it. This is the exact `apply_transactional` discipline.
async fn check_transaction_visibility<S: SqlSession>(
    session: &S,
    scratch_table: &str,
    fixture: &SeamFixture,
) -> Result<(), ConformanceFailure> {
    const CHECK: &str = "transaction-visibility";
    let temp = fixture.temp_keyword;
    let int8 = fixture.bigint_type;
    let p1 = (fixture.placeholder)(1);
    let count_n = (fixture.as_bigint)("count(*)");
    session
        .batch(&format!("CREATE {temp} TABLE {scratch_table} (id {int8})"))
        .await
        .map_err(|e| fail(CHECK, format!("create scratch table: {e}")))?;
    session
        .batch("BEGIN")
        .await
        .map_err(|e| fail(CHECK, format!("BEGIN: {e}")))?;
    session
        .exec(
            &format!("INSERT INTO {scratch_table} (id) VALUES ({p1})"),
            &[Bind::Int(99)],
        )
        .await
        .map_err(|e| fail(CHECK, format!("in-txn INSERT: {e}")))?;
    // Visible inside the open txn on the SAME session.
    let in_txn = session
        .query_one(&format!("SELECT {count_n} AS n FROM {scratch_table}"), &[])
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
        .query_one(&format!("SELECT {count_n} AS n FROM {scratch_table}"), &[])
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

/// Check 3 - bind-inference semantics: a param with NO declared type is coerced by
/// the server to the target column type (the `text -> timestamptz` path), and a
/// `None` text param is a SQL NULL. This is the load-bearing distinction between
/// a DECLARED bind and an inferred one.
async fn check_bind_inference_semantics<S: SqlSession>(
    session: &S,
    fixture: &SeamFixture,
) -> Result<(), ConformanceFailure> {
    const CHECK: &str = "bind-inference-semantics";
    let temp = fixture.temp_keyword;
    let int8 = fixture.bigint_type;
    let ts_ty = fixture.timestamp_type;
    let p1 = (fixture.placeholder)(1);
    let p2 = (fixture.placeholder)(2);
    let p3 = (fixture.placeholder)(3);
    // A scratch temp table with a timestamp column: the coercion the engine's
    // op.* DML path relies on (a text instant -> the server's timestamp type).
    session
        .batch(&format!(
            "CREATE {temp} TABLE zm_conf_text (id {int8}, ts {ts_ty}, tag text)"
        ))
        .await
        .map_err(|e| fail(CHECK, format!("create text-coercion scratch table: {e}")))?;
    // All-inferred: every param crosses with NO declared type. The id becomes an
    // integer, the instant becomes a timestamp, `Inferred(None)` becomes SQL NULL.
    // A DECLARED text bind against a timestamp column is REFUSED on at least one
    // supported server - this path must not be.
    let affected = session
        .exec(
            &format!("INSERT INTO zm_conf_text (id, ts, tag) VALUES ({p1}, {p2}, {p3})"),
            &[
                Bind::Inferred(Some("7".to_string())),
                Bind::Inferred(Some(fixture.timestamp_text_param.to_string())),
                Bind::Inferred(None),
            ],
        )
        .await
        .map_err(|e| {
            fail(
                CHECK,
                format!("all-inferred INSERT with a text-to-timestamp coercion failed: {e}"),
            )
        })?;
    if affected != 1 {
        return Err(fail(
            CHECK,
            format!("all-inferred exec reported {affected} rows affected, expected 1"),
        ));
    }
    // Read back: the id coerced to int8, the tag is a genuine SQL NULL.
    // `ts_ok` comes back as an INTEGER 1/0 rather than a boolean: the two servers
    // do not agree on how a boolean crosses the wire, and the property under test
    // is the coercion, not the boolean encoding.
    // `CASE WHEN` rather than a direct cast of the comparison: at least one
    // supported server has no boolean-to-integer cast, and CASE is understood by
    // every one of them.
    let ts_ok_expr = (fixture.as_bigint)(&format!(
        "CASE WHEN {} THEN 1 ELSE 0 END",
        (fixture.ts_matches)()
    ));
    let row = session
        .query_one(
            &format!("SELECT id, tag, {ts_ok_expr} AS ts_ok FROM zm_conf_text WHERE id = 7"),
            &[],
        )
        .await
        .map_err(|e| {
            fail(
                CHECK,
                format!("read-back after the all-inferred insert: {e}"),
            )
        })?;
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
    let ts_ok: i64 = row
        .try_get("ts_ok")
        .map_err(|e| fail(CHECK, format!("decode ts coercion flag: {e}")))?;
    if ts_ok != 1 {
        return Err(fail(
            CHECK,
            "the text `ts` param did not coerce to the expected timestamptz value",
        ));
    }
    // Also assert `exec` (typed binds) round-trips a NULL Bind on a text column -
    // the exact shape the shipped `exec` path binds a NULL (a nullable text
    // `last_cursor`, `backfill.rs`), never against a timestamp column (that path
    // is an inferred bind). A `Bind::Null` must land as a SQL NULL and read back as
    // `Option::None`.
    let n = session
        .exec(
            &format!("INSERT INTO zm_conf_text (id, ts, tag) VALUES ({p1}, now(), {p2})"),
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
    // MIXED binds in ONE statement: a DECLARED integer key, an INFERRED instant,
    // and a DECLARED text tag. This is the case the seam could not express while
    // untypedness was a property of the verb rather than of the value - the old
    // all-or-nothing spelling forced the key and the tag to go untyped too.
    //
    // It is also the sharpest check in this suite, because it fails in BOTH
    // directions: declare the instant and the server refuses the coercion, infer
    // everything and a driver that silently drops declared types still passes.
    let mixed = session
        .exec(
            &format!("INSERT INTO zm_conf_text (id, ts, tag) VALUES ({p1}, {p2}, {p3})"),
            &[
                Bind::Int(9),
                Bind::Inferred(Some(fixture.timestamp_text_param.to_string())),
                Bind::Text("mixed".to_string()),
            ],
        )
        .await
        .map_err(|e| {
            fail(
                CHECK,
                format!("exec mixing a typed key with an inferred instant failed: {e}"),
            )
        })?;
    if mixed != 1 {
        return Err(fail(
            CHECK,
            format!("mixed-bind exec reported {mixed} rows, expected 1"),
        ));
    }
    let mixed_row = session
        .query_one(
            &format!("SELECT tag, {ts_ok_expr} AS ts_ok FROM zm_conf_text WHERE id = 9"),
            &[],
        )
        .await
        .map_err(|e| fail(CHECK, format!("read-back mixed-bind row: {e}")))?;
    let mixed_tag: Option<String> = mixed_row
        .try_get("tag")
        .map_err(|e| fail(CHECK, format!("decode mixed-bind tag: {e}")))?;
    if mixed_tag.as_deref() != Some("mixed") {
        return Err(fail(
            CHECK,
            format!("mixed-bind tag round-tripped as {mixed_tag:?}, expected \"mixed\""),
        ));
    }
    let mixed_ts_ok: i64 = mixed_row
        .try_get("ts_ok")
        .map_err(|e| fail(CHECK, format!("decode mixed-bind ts flag: {e}")))?;
    if mixed_ts_ok != 1 {
        return Err(fail(
            CHECK,
            "the inferred instant in a mixed-bind statement did not reach the \
             timestamp column intact",
        ));
    }
    session
        .batch("DROP TABLE zm_conf_text")
        .await
        .map_err(|e| fail(CHECK, format!("drop text-coercion scratch table: {e}")))?;
    Ok(())
}

/// Check 4 - error + SQLSTATE mapping: a statement that fails at the server
/// surfaces a [`DbError`] with a non-empty message and (when the driver carries it)
/// the real server SQLSTATE. A driver that swallows the error, or stringifies a
/// panic, fails here.
async fn check_error_sqlstate_mapping<S: SqlSession>(
    session: &S,
    fixture: &SeamFixture,
) -> Result<(), ConformanceFailure> {
    const CHECK: &str = "error-sqlstate-mapping";
    let want_state = fixture.undefined_table_sqlstate;
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
    // The SQLSTATE is optional in the seam, but a driver that surfaces one
    // MUST surface the real code. 42P01 = undefined_table. We accept either "the
    // driver carries no sqlstate" (message-only is a valid seam contract) or "it
    // carries the correct one" - but a WRONG non-empty sqlstate is a bug.
    if let Some(state) = &err.sqlstate {
        if state != want_state {
            return Err(fail(
                CHECK,
                format!("expected SQLSTATE {want_state} (undefined_table), got {state:?}"),
            ));
        }
    }
    // Prove the session is still usable after the error (the driver did not wedge
    // the pinned connection): a trivial query still runs and decodes.
    let alive = session
        .query_one(&format!("SELECT {} AS one", (fixture.as_bigint)("1")), &[])
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
