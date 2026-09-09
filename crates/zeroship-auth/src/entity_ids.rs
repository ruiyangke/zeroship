//! Reading platform entity ids out of PostgreSQL rows.
//!
//! `zeroship.users.id`, `zeroship.apps.id` and every foreign-key copy of the two
//! are `text COLLATE "C"` holding a printed typed id, so a row read is a parse
//! and a parse can fail. Neither [`UserId`] nor [`AppId`] implements `ToSql` or
//! `FromSql`: a bind goes through `as_str`, and a read goes through the helpers
//! here.
//!
//! The parse failure is reported as [`AuthError::Db`] and names the column,
//! which is how the surrounding stores report every other malformed row. A
//! caller in a production path must never unwrap it: the value is whatever the
//! column holds, and a row that does not carry a canonical id is a fault to
//! report rather than a panic to take.

use compio_postgres::Row;
use zeroship_core::app_id::AppId;
use zeroship_core::user_id::UserId;

use crate::error::{AuthError, Result};

/// Read a non-null `usr_<base62>` column.
///
/// # Errors
///
/// [`AuthError::Db`] when the column is absent, null, or holds a value the
/// typed-id parser refuses.
pub fn user_id(row: &Row, column: &str) -> Result<UserId> {
    let raw: &str = row
        .try_get(column)
        .map_err(|err| AuthError::Db(format!("read {column}: {err}")))?;
    parse_user_id(raw, column)
}

/// Read a nullable `usr_<base62>` column. A SQL NULL is `Ok(None)`; a present
/// but unparseable value is still an error.
///
/// # Errors
///
/// [`AuthError::Db`] when the column is absent or holds a value the typed-id
/// parser refuses.
pub fn optional_user_id(row: &Row, column: &str) -> Result<Option<UserId>> {
    let raw: Option<&str> = row
        .try_get(column)
        .map_err(|err| AuthError::Db(format!("read {column}: {err}")))?;
    raw.map(|raw| parse_user_id(raw, column)).transpose()
}

/// Read a non-null `app_<base62>` column.
///
/// # Errors
///
/// [`AuthError::Db`] when the column is absent, null, or holds a value the
/// typed-id parser refuses.
pub fn app_id(row: &Row, column: &str) -> Result<AppId> {
    let raw: &str = row
        .try_get(column)
        .map_err(|err| AuthError::Db(format!("read {column}: {err}")))?;
    parse_app_id(raw, column)
}

/// Read a nullable `app_<base62>` column. A SQL NULL is `Ok(None)`; a present
/// but unparseable value is still an error.
///
/// # Errors
///
/// [`AuthError::Db`] when the column is absent or holds a value the typed-id
/// parser refuses.
pub fn optional_app_id(row: &Row, column: &str) -> Result<Option<AppId>> {
    let raw: Option<&str> = row
        .try_get(column)
        .map_err(|err| AuthError::Db(format!("read {column}: {err}")))?;
    raw.map(|raw| parse_app_id(raw, column)).transpose()
}

/// Parse a user id that arrived from somewhere other than a row - a URL
/// segment, a signed stash, a token subject.
///
/// # Errors
///
/// [`AuthError::Db`] naming `origin`, so the message says which input carried
/// the value rather than only that one did.
pub fn parse_user_id(raw: &str, origin: &str) -> Result<UserId> {
    UserId::parse(raw).map_err(|err| AuthError::Db(format!("{origin} is not a user id: {err}")))
}

/// Parse an app id that arrived from somewhere other than a row.
///
/// # Errors
///
/// [`AuthError::Db`] naming `origin`.
pub fn parse_app_id(raw: &str, origin: &str) -> Result<AppId> {
    AppId::parse(raw).map_err(|err| AuthError::Db(format!("{origin} is not an app id: {err}")))
}
