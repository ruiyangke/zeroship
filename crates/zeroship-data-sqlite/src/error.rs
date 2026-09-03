//! SQLite-side error mapping.
//!
//! This module implements a SQLSTATE-equivalent switch over
//! `rusqlite::Error` produces typed [`DbError`] variants whose
//! `.code` matches the PG-side codes the SDK already branches on
//! (`unique_violation`, `fk_violation`, `not_null_violation`,
//! `check_violation`, `lock_not_available`, `transient`). The wire
//! shape stays uniform across backends, so SDK callers don't need to
//! special-case SQLite.
//!
//! **Extended result codes** (the second u8 in `extended_code`)
//! carry sub-classifications PG's SQLSTATE never had to encode —
//! e.g. `SQLITE_BUSY` (5) vs `SQLITE_BUSY_RECOVERY` (261) vs
//! `SQLITE_BUSY_TIMEOUT` (773) all collapse into a single
//! `LockContention` here, matching the PG `LOCK_NOT_AVAILABLE`
//! mapping. The wire `.code` is the same; only the message body
//! preserves the extended-code detail for operator logs.
//!
//! Sources: SQLite extended result codes:
//! https://www.sqlite.org/rescode.html#extrc — the integer constants
//! used in the match arms below are the canonical values, NOT pulled
//! from `libsqlite3_sys` constants (which would force a `use` for
//! each — the integers are part of the SQLite stable ABI).

use zeroship_data_core::error::DbError;

// ---------------------------------------------------------------------------
// Extended result-code constants. The values are part of SQLite's
// public ABI (https://www.sqlite.org/rescode.html#extrc) — pinning
// them as named consts here lets the match arms read as a SQLSTATE
// table and makes the code grep-friendly for operators correlating
// against the SQLite docs.
// ---------------------------------------------------------------------------

const SQLITE_BUSY: i32 = 5;
const SQLITE_BUSY_RECOVERY: i32 = 261;
const SQLITE_BUSY_SNAPSHOT: i32 = 517;
const SQLITE_BUSY_TIMEOUT: i32 = 773;

/// `SQLITE_INTERRUPT` (9). The result code `sqlite3_interrupt` produces, and
/// therefore the one every platform-issued cancellation arrives as.
///
/// It has no extended variants, so the primary code is the whole set. Until
/// SC-2 this string appeared **zero** times anywhere in `crates/` or `libs/`,
/// which meant a cancellation the platform asked for fell through to the
/// catch-all and surfaced as `Transient` - indistinguishable from a real fault
/// at exactly the moment an operator is trying to understand a timeout.
const SQLITE_INTERRUPT: i32 = 9;

const SQLITE_CONSTRAINT_CHECK: i32 = 275;
const SQLITE_CONSTRAINT_FOREIGNKEY: i32 = 787;
const SQLITE_CONSTRAINT_NOTNULL: i32 = 1299;
const SQLITE_CONSTRAINT_UNIQUE: i32 = 2067;

/// Map a `rusqlite::Error` into a typed [`DbError`].
///
/// The mapping mirrors `zeroship_data_postgres::pg_error::classify`'s SQLSTATE
/// switch — every variant the SDK branches on has a SQLite counter-
/// part below. The message body always includes the rusqlite display
/// and the extended code so operators can correlate against the SQLite
/// docs without losing the typed `.code` for SDK callers.
pub(crate) fn from_sqlite(e: rusqlite::Error) -> DbError {
    // Capture the human-readable message once so every arm can reuse
    // it without re-formatting. `to_string()` on `rusqlite::Error`
    // already renders the inner `SqliteFailure` body when applicable.
    let msg = format!("db: {e}");

    match &e {
        // SQLITE_INTERRUPT — a cancellation the platform asked for, not a
        // fault. Without this arm it falls through to the catch-all and
        // surfaces as `Transient`, indistinguishable from a real fault at
        // exactly the moment an operator is trying to understand a timeout.
        //
        // Its position among the other arms is NOT load-bearing - 9 is in no
        // other arm's set - so do not read the ordering as a guard. What is
        // load-bearing is that the wire code equals
        // [`crate::reservation::CANCELLED_CODE`], the code the
        // actor's terminal classifier reports: a caller cannot tell, and does
        // not need to tell, which of the two produced it.
        rusqlite::Error::SqliteFailure(ffi_err, _) if ffi_err.extended_code == SQLITE_INTERRUPT => {
            DbError::Coded {
                code: crate::reservation::CANCELLED_CODE.to_string(),
                message: msg,
                hint: None,
            }
        }

        // SQLITE_BUSY family — the SQLite analogue of PG's
        // LOCK_NOT_AVAILABLE / OBJECT_IN_USE. The SDK already
        // branches on `code = "lock_not_available"` for this class;
        // mirroring keeps the contract uniform.
        rusqlite::Error::SqliteFailure(ffi_err, _)
            if matches!(
                ffi_err.extended_code,
                SQLITE_BUSY | SQLITE_BUSY_RECOVERY | SQLITE_BUSY_SNAPSHOT | SQLITE_BUSY_TIMEOUT
            ) =>
        {
            DbError::LockContention { message: msg }
        }

        // Unique violation — wire `.code = "unique_violation"`,
        // wrapped in a SchemaRefused envelope so the SDK's
        // existing PG-side unique-violation parser sees a uniform
        // wire payload across backends. The envelope mirrors the
        // `cic_failed` shape the PG IndexBuilder emits at
        // `crates/plugin-db/src/backend/postgres.rs:442`.
        rusqlite::Error::SqliteFailure(ffi_err, _)
            if ffi_err.extended_code == SQLITE_CONSTRAINT_UNIQUE =>
        {
            DbError::SchemaRefused {
                code: "unique_violation",
                envelope_json: build_constraint_envelope("unique_violation", &msg),
            }
        }

        // Foreign-key violation — wire `.code = "fk_violation"`.
        rusqlite::Error::SqliteFailure(ffi_err, _)
            if ffi_err.extended_code == SQLITE_CONSTRAINT_FOREIGNKEY =>
        {
            DbError::SchemaRefused {
                code: "fk_violation",
                envelope_json: build_constraint_envelope("fk_violation", &msg),
            }
        }

        // NOT NULL violation — wire `.code = "not_null_violation"`.
        rusqlite::Error::SqliteFailure(ffi_err, _)
            if ffi_err.extended_code == SQLITE_CONSTRAINT_NOTNULL =>
        {
            DbError::SchemaRefused {
                code: "not_null_violation",
                envelope_json: build_constraint_envelope("not_null_violation", &msg),
            }
        }

        // CHECK violation — wire `.code = "check_violation"`.
        rusqlite::Error::SqliteFailure(ffi_err, _)
            if ffi_err.extended_code == SQLITE_CONSTRAINT_CHECK =>
        {
            DbError::SchemaRefused {
                code: "check_violation",
                envelope_json: build_constraint_envelope("check_violation", &msg),
            }
        }

        // QueryReturnedNoRows is the rusqlite equivalent of the PG
        // empty-RETURNING case. PG's `classify` doesn't have a peer
        // — empty-RETURNING surfaces via `first_row_or_internal` —
        // but on SQLite the driver raises this variant explicitly
        // from `query_row` / `query_one`. Map to Internal so the
        // shape stays uniform with PG's empty-RETURNING handling;
        // SDK callers see the same `code = "internal"` flag.
        rusqlite::Error::QueryReturnedNoRows => DbError::Internal { message: msg },

        // Invalid column name / type — purely defensive, indicates
        // an introspection-side bug (we asked for a column the
        // query didn't return, or the value's type didn't match the
        // expected `T`). Wire `.code = "internal"` matches PG's
        // catch-all for "this should never happen".
        rusqlite::Error::InvalidColumnName(_) | rusqlite::Error::InvalidColumnType(_, _, _) => {
            DbError::Internal { message: msg }
        }

        // Default arm: any other `SqliteFailure` extended code (I/O
        // errors, corruption, schema-changed, etc.) collapses to
        // `Transient` — the same bucket PG uses for class 08
        // / 53 / 57. Wire `.code = "transient"`; the SDK already
        // exposes a backoff-and-retry path for this class.
        _ => DbError::Transient { message: msg },
    }
}

/// Build the wire envelope for the four SchemaRefused constraint
/// codes. Matches the PG IndexBuilder's `cic_failed` envelope shape
/// at `crates/plugin-db/src/backend/postgres.rs:442` so the SDK's
/// existing PG-side parser handles the SQLite arm unchanged.
fn build_constraint_envelope(code: &str, message: &str) -> String {
    serde_json::to_string(&serde_json::json!({
        "code": code,
        "message": message,
    }))
    .unwrap_or_else(|_| {
        // `to_string` on a Value composed of static strings is
        // infallible in practice; the fallback keeps the function
        // total without a panic on an unreachable branch.
        format!("{{\"code\":\"{code}\",\"reason\":\"envelope serialisation failed\"}}")
    })
}

#[cfg(test)]
mod tests {
    //! Unit tests pin the SQLSTATE-equivalent classification table
    //! against the named extended-result-code constants. The tests
    //! synthesise `rusqlite::Error` values via the public `ffi::Error`
    //! constructor; no live SQLite needed.

    use super::*;

    fn synth(extended_code: i32) -> rusqlite::Error {
        // `ffi::Error::new(code)` derives the primary `code` field
        // from the low byte; the high bits stay in `extended_code`,
        // which is what our match arms inspect.
        let ffi = rusqlite::ffi::Error::new(extended_code);
        rusqlite::Error::SqliteFailure(ffi, Some("synthesised".to_string()))
    }

    #[test]
    fn busy_family_collapses_to_lock_contention() {
        for code in [
            SQLITE_BUSY,
            SQLITE_BUSY_RECOVERY,
            SQLITE_BUSY_SNAPSHOT,
            SQLITE_BUSY_TIMEOUT,
        ] {
            let db = from_sqlite(synth(code));
            match db {
                DbError::LockContention { .. } => {}
                other => panic!("code {code} should be LockContention, got {other:?}"),
            }
        }
    }

    #[test]
    fn unique_violation_maps_to_schema_refused_with_unique_code() {
        let db = from_sqlite(synth(SQLITE_CONSTRAINT_UNIQUE));
        match db {
            DbError::SchemaRefused {
                code,
                envelope_json,
            } => {
                assert_eq!(code, "unique_violation");
                let v: serde_json::Value =
                    serde_json::from_str(&envelope_json).expect("envelope must be valid JSON");
                assert_eq!(v["code"], "unique_violation");
            }
            other => panic!("expected SchemaRefused, got {other:?}"),
        }
    }

    #[test]
    fn fk_violation_maps_to_schema_refused_with_fk_code() {
        let db = from_sqlite(synth(SQLITE_CONSTRAINT_FOREIGNKEY));
        match db {
            DbError::SchemaRefused { code, .. } => assert_eq!(code, "fk_violation"),
            other => panic!("expected SchemaRefused, got {other:?}"),
        }
    }

    #[test]
    fn not_null_violation_maps_to_schema_refused_with_not_null_code() {
        let db = from_sqlite(synth(SQLITE_CONSTRAINT_NOTNULL));
        match db {
            DbError::SchemaRefused { code, .. } => assert_eq!(code, "not_null_violation"),
            other => panic!("expected SchemaRefused, got {other:?}"),
        }
    }

    #[test]
    fn check_violation_maps_to_schema_refused_with_check_code() {
        let db = from_sqlite(synth(SQLITE_CONSTRAINT_CHECK));
        match db {
            DbError::SchemaRefused { code, .. } => assert_eq!(code, "check_violation"),
            other => panic!("expected SchemaRefused, got {other:?}"),
        }
    }

    #[test]
    fn query_returned_no_rows_maps_to_internal() {
        let db = from_sqlite(rusqlite::Error::QueryReturnedNoRows);
        match db {
            DbError::Internal { .. } => {}
            other => panic!("expected Internal, got {other:?}"),
        }
    }

    #[test]
    fn invalid_column_name_maps_to_internal() {
        let db = from_sqlite(rusqlite::Error::InvalidColumnName("bogus".to_string()));
        match db {
            DbError::Internal { .. } => {}
            other => panic!("expected Internal, got {other:?}"),
        }
    }

    /// The arm SC-2 records as absent: without it a cancellation the platform
    /// asked for is reported as an opaque `Transient` database error.
    #[test]
    fn interrupt_maps_to_the_cancellation_code_not_a_transient_fault() {
        // The LITERAL 9, not `SQLITE_INTERRUPT`. Feeding the constant to the
        // arm that matches on it makes the test measure nothing: mutating the
        // constant to an unreachable value moves the input with the matcher
        // and the test stays green. Verified against
        // https://www.sqlite.org/rescode.html - `SQLITE_INTERRUPT` is 9, a
        // primary result code with no extended variants.
        assert_eq!(SQLITE_INTERRUPT, 9, "SQLITE_INTERRUPT is 9 in SQLite's ABI");
        let db = from_sqlite(synth(9));
        match db {
            DbError::Coded { code, .. } => assert_eq!(
                code,
                crate::reservation::CANCELLED_CODE,
                "SQLITE_INTERRUPT must report the cancellation code"
            ),
            other => {
                panic!("SQLITE_INTERRUPT must not fall through to the catch-all; got {other:?}")
            }
        }
    }

    /// The control that differs in one variable: a neighbouring result code
    /// with no cancellation meaning must still be `Transient`. Without this,
    /// an arm that matched everything would pass the test above.
    #[test]
    fn a_neighbouring_result_code_is_still_transient() {
        // SQLITE_LOCKED (6) and SQLITE_NOMEM (7) bracket SQLITE_INTERRUPT (9)
        // without being cancellations. SQLITE_BUSY (5) has its own arm.
        for code in [6, 7, 10] {
            match from_sqlite(synth(code)) {
                DbError::Transient { .. } => {}
                other => panic!("code {code} must stay Transient, got {other:?}"),
            }
        }
    }

    #[test]
    fn unknown_sqlite_failure_maps_to_transient() {
        // SQLITE_CORRUPT (11) — not in any of our specific buckets.
        let db = from_sqlite(synth(11));
        match db {
            DbError::Transient { .. } => {}
            other => panic!("expected Transient, got {other:?}"),
        }
    }
}
