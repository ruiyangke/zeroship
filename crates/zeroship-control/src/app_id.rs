use zeroship_core::AppId;

use crate::registry::RegistryError;

pub(crate) fn parse_stored(raw: &str, context: &str) -> Result<AppId, RegistryError> {
    AppId::parse(raw).map_err(|error| {
        RegistryError::Database(format!("{context}: invalid stored app id: {error}"))
    })
}

pub(crate) fn from_row(
    row: &compio_postgres::Row,
    column: &str,
    context: &str,
) -> Result<AppId, RegistryError> {
    let raw = row
        .try_get::<_, String>(column)
        .map_err(|error| RegistryError::Database(format!("{context}: read app id: {error}")))?;
    parse_stored(&raw, context)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn stored_app_id_parser_rejects_raw_uuid_with_context() {
        let error = parse_stored("0197f8a1-2b3c-7d4e-8f90-1a2b3c4d5e6f", "invoice line")
            .expect_err("raw UUID must not decode as an app identity");
        assert!(error.to_string().contains("invoice line"));
    }
}
