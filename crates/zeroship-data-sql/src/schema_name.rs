//! Validated physical schema identity, separate from the app's tenant identity.
//!
//! Database qualification and PostgreSQL role setup use this type. Construction
//! through `SchemaName::new` checks identifier syntax and limits before execution;
//! explicit accessors make conversion back to text visible at call sites.

use crate::compile::{QueryError, quote_ident, validate_schema};

/// A validated physical database schema name.
///
/// Constructed only through [`SchemaName::new`]; see the module docs for why
/// there is no infallible route in.
#[derive(Clone, Debug, Eq, Hash, Ord, PartialEq, PartialOrd)]
pub struct SchemaName(String);

impl SchemaName {
    /// Validate `name` as a physical schema identifier and wrap it.
    ///
    /// # Errors
    ///
    /// Returns the same [`QueryError::InvalidCollection`] value that
    /// `crate::compile::validate_schema` has always produced for an empty name
    /// or one carrying a character outside `[A-Za-z0-9_-]`.
    pub fn new(name: &str) -> Result<Self, QueryError> {
        validate_schema(name)?;
        Ok(Self(name.to_string()))
    }

    /// The schema name as written, unquoted.
    ///
    /// Named rather than an `AsRef`/`Deref` impl so that every place a schema
    /// name degrades back into an untyped string is greppable.
    pub fn as_str(&self) -> &str {
        &self.0
    }

    /// The schema name as a double-quoted SQL identifier.
    pub fn quoted(&self) -> String {
        quote_ident(&self.0)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn new_accepts_the_identifier_shapes_the_platform_actually_mints() {
        for name in [
            "app_demo",
            "0191e7a2-b3c4-4d5e-8f90-123456789abc",
            "db_0191e7a2b3c44d5e8f90123456789abc",
        ] {
            assert_eq!(
                SchemaName::new(name).expect("legal schema name").as_str(),
                name,
                "the wrapper must preserve the name byte for byte"
            );
        }
    }

    /// The refusal is the pre-existing one, verbatim. If this drifts, an illegal
    /// name that used to be refused per operation is refused differently at the
    /// mint, and the two are no longer the same predicate.
    #[test]
    fn new_refuses_exactly_what_validate_schema_refuses_and_says_the_same_thing() {
        for illegal in [
            "",
            "app\"; DROP SCHEMA public; --",
            "app.public",
            "app id",
            "\u{e9}pp",
        ] {
            let direct = validate_schema(illegal).expect_err("validate_schema must refuse");
            let minted = SchemaName::new(illegal).expect_err("SchemaName::new must refuse");
            assert_eq!(
                minted.to_string(),
                direct.to_string(),
                "the mint must not invent a second refusal message for {illegal:?}"
            );
        }
    }

    /// The control for the arm above: a name `validate_schema` accepts is not
    /// refused by the mint either, so that test is measuring the predicate
    /// rather than a constructor that refuses everything.
    #[test]
    fn new_accepts_exactly_what_validate_schema_accepts() {
        for legal in ["a", "A-1", "_", "app_demo"] {
            assert!(validate_schema(legal).is_ok(), "control fixture {legal:?}");
            assert!(SchemaName::new(legal).is_ok(), "{legal:?}");
        }
    }

    #[test]
    fn quoted_is_the_shared_identifier_quoting() {
        let schema = SchemaName::new("app-demo").expect("legal schema name");
        assert_eq!(schema.quoted(), quote_ident("app-demo"));
        assert_eq!(schema.quoted(), "\"app-demo\"");
    }
}
