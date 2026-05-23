//! SQLite-side error mapping — stub.
//!
//! **P1 PR 1**: skeleton only. The real classifier (switch on
//! `rusqlite::Error::SqliteFailure(extended_code, _)` → typed
//! `DbError` variants per design §15.7) lands in PR 2 alongside the
//! `SqliteSession` actor that surfaces these errors.

use crate::error::DbError;

/// Map a `rusqlite::Error` into a typed [`DbError`].
///
/// **P1 PR 1 stub**: returns `DbError::Internal` with a sentinel
/// message. PR 2 replaces the body with a SQLSTATE-style switch over
/// the extended result codes — unique / fk / not-null / check
/// violations → typed variants the SDK can branch on without
/// re-parsing the message.
pub(crate) fn from_sqlite(_e: rusqlite::Error) -> DbError {
    DbError::Internal {
        message: "sqlite error mapping not implemented (P1 PR2+ stub)".into(),
    }
}
