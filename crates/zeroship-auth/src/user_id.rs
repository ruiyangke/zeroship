use zeroship_core::UserId;

use crate::error::{AuthError, Result};

pub(crate) fn parse_stored(raw: &str, context: &str) -> Result<UserId> {
    UserId::parse(raw)
        .map_err(|error| AuthError::Db(format!("{context}: invalid stored user id: {error}")))
}

pub(crate) fn from_row(
    row: &compio_postgres::Row,
    column: &str,
    context: &str,
) -> Result<UserId> {
    let raw = row
        .try_get::<_, String>(column)
        .map_err(|error| AuthError::Db(format!("{context}: read user id: {error}")))?;
    parse_stored(&raw, context)
}
