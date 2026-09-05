//! [`SchemaName`] - the physical database schema an app's tables live in.
//!
//! # Why this is a type and not a `&str`
//!
//! One string is presently the tenant identity AND the physical schema name AND
//! the role-name stem AND the encryption salt AND the publication key AND the
//! replication-slot key AND the SQLite `ATTACH` alias AND the transaction-lane
//! key AND the broker routing key AND the CDC event stamp. Every one of those
//! sites compiles with *either* meaning, so the day the schema becomes
//! `db_<dbsid>` while the tenant stays the app uuid, a site that kept the wrong
//! one is silently wrong rather than broken.
//!
//! The failure that motivated this type is not hypothetical. The migration
//! service composes the runtime role from the SCHEMA it created
//! (`zeroship_migrate_server::apply::runtime_role_provisioning_sql`) while the
//! data plane's `SET LOCAL ROLE` composes it from whatever its caller happened
//! to be holding (`zeroship_data_postgres::pg_session_sql::tx_session_setup_sql`).
//! Both took `&str`. Diverge the two identities and every transaction fails at
//! session setup, and - worse - the classifier that turns that failure into an
//! actionable `SCHEMA_NOT_PROVISIONED` also derives its expected role from a
//! `&str`, so it stops matching and the creator gets a generic failure with no
//! instruction to run `zeroship migrate`.
//!
//! # What this type deliberately does NOT have
//!
//! No `Deref`, no `From<&str>`, no `AsRef<str>`, no public inner field, and no
//! `new_unchecked`. Each of those is precisely a route by which a tenant id
//! reaches a parameter that wants a schema without anyone deciding that it
//! should. Construction is only [`SchemaName::new`], which is fallible, and
//! extraction is only the two named accessors below.
//!
//! # The validation is the existing one, unchanged
//!
//! [`SchemaName::new`] delegates to [`crate::query::validate_schema`] - the same
//! predicate every `build_*` function called per operation before this type
//! existed - and returns the same [`QueryError`] values byte for byte. This
//! moves *when* an illegal name is refused (once, at construction) without
//! changing *what* is refused or what the refusal says.

use crate::query::{QueryError, quote_ident, validate_schema};

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
    /// [`crate::query::validate_schema`] has always produced for an empty name
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
