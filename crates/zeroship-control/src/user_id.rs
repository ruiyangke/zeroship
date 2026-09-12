use zeroship_core::UserId;

use crate::registry::RegistryError;

pub(crate) fn parse_stored(raw: &str, context: &str) -> Result<UserId, RegistryError> {
    UserId::parse(raw).map_err(|error| {
        RegistryError::Database(format!("{context}: invalid stored user id: {error}"))
    })
}

pub(crate) fn from_row(
    row: &compio_postgres::Row,
    column: &str,
    context: &str,
) -> Result<UserId, RegistryError> {
    let raw = row
        .try_get::<_, String>(column)
        .map_err(|error| RegistryError::Database(format!("{context}: read user id: {error}")))?;
    parse_stored(&raw, context)
}

pub(crate) fn optional_from_row(
    row: &compio_postgres::Row,
    column: &str,
    context: &str,
) -> Result<Option<UserId>, RegistryError> {
    let raw = row.try_get::<_, Option<String>>(column).map_err(|error| {
        RegistryError::Database(format!("{context}: read optional user id: {error}"))
    })?;
    raw.as_deref()
        .map(|raw| parse_stored(raw, context))
        .transpose()
}
