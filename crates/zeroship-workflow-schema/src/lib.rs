//! The workflow journal's generated schema artifacts, and nothing else.
//!
//! The journal is a PLATFORM-owned schema that lives inside a creator database.
//! Two parties need to read its artifacts and they must not reach each other to
//! do it: the workflow engine (which runs inside a worker) and whichever service
//! installs the schema. This crate is the shared leaf they both depend on. It
//! depends on nothing else in the workspace, so depending on it cannot drag the
//! engine into a platform service.
//!
//! `schema/schema.ts` remains the one authored source; `schema/generate.mjs`
//! compiles it through the migration compiler into the artifacts embedded here.
//!
//! # The schema-name substitution lives here, once
//!
//! The generated `PostgreSQL` DDL names its schema with the placeholder
//! [`SCHEMA_PLACEHOLDER`], which the installer replaces with the schema the
//! journal is installed into. That replacement used to be written twice - once
//! in the migration service and once in the engine - two copies that had to
//! agree exactly with nothing holding them together. [`postgres_sql`] is now the
//! only one.
//!
//! # The schema is not an app
//!
//! Nothing here takes an app id, and no name here should suggest one. A journal
//! belongs to a creator database, and one schema holds the journals of every app
//! in it - the `app_id` COLUMNS inside the journal are the tenant discriminator,
//! and [`STAMP_ROW_ID`] is one row per journal, not one per app.

/// The generated `PostgreSQL` DDL, still carrying [`SCHEMA_PLACEHOLDER`].
///
/// Callers want [`postgres_sql`]; this is public so a test can assert against
/// the unbound form.
pub const POSTGRES_TEMPLATE: &str = include_str!("../schema/postgres.sql");

/// The generated `SQLite` DDL.
///
/// `SQLite` has one schema per file, so there is nothing to substitute and the
/// artifact is applied verbatim.
pub const SQLITE_SQL: &str = include_str!("../schema/sqlite.sql");

/// The generated v2 `RuntimeSchemaDescriptor` for the journal's collections.
pub const RUNTIME_DESCRIPTOR_JSON: &str = include_str!("../schema/schema.runtime.json");

/// Per-dialect sha256 of the generated DDL, as a JSON object.
const FINGERPRINTS_JSON: &str = include_str!("../schema/fingerprints.json");

/// The quoted identifier the generated `PostgreSQL` DDL uses for the schema it is
/// not yet bound to.
///
/// It is the QUOTED form on purpose: substituting a bare word would also rewrite
/// the same characters appearing inside a string literal.
pub const SCHEMA_PLACEHOLDER: &str = "\"__zeroship_workflow_schema\"";

/// The unqualified name of the table that stamps which journal is installed.
pub const STAMP_TABLE: &str = "__zeroship_workflow_schema_version";

/// The stamp row this schema owns, inside [`STAMP_TABLE`].
///
/// ONE row per journal. It is not keyed by anything, and in particular not by an
/// app: every app whose workflows live in this schema is described by this single
/// row, so installing or upgrading the journal moves all of them at once.
pub const STAMP_ROW_ID: &str = "workflow";

/// Which journal the embedded artifacts describe.
///
/// The artifacts are a snapshot of one point in an ordered series. An installer
/// records this number beside the fingerprint so a later platform can tell an
/// out-of-date journal from a corrupted one, and so an older platform can
/// refuse to write over a newer journal.
pub const VERSION: u32 = 1;

/// The dialect key for `PostgreSQL` artifacts.
pub const POSTGRES: &str = "postgres";

/// The dialect key for `SQLite` artifacts.
pub const SQLITE: &str = "sqlite";

/// Quote one identifier for either dialect, doubling any embedded quote.
///
/// Spelled here rather than borrowed so this crate stays a leaf. It is the same
/// rule `zeroship_data_orm::sql::mapping::quote_ident` applies; the parity is
/// asserted by `substitution_matches_the_orm_quoting_rule` below.
#[must_use]
pub fn quote_ident(ident: &str) -> String {
    format!("\"{}\"", ident.replace('"', "\"\""))
}

/// The journal DDL bound to the physical schema it is installed into.
#[must_use]
pub fn postgres_sql(schema: &str) -> String {
    POSTGRES_TEMPLATE.replace(SCHEMA_PLACEHOLDER, &quote_ident(schema))
}

/// The fingerprint a host compares an installed journal against.
///
/// `None` for a dialect this schema was not generated for.
#[must_use]
pub fn fingerprint(dialect: &str) -> Option<&'static str> {
    fingerprints()
        .get(dialect)
        .and_then(serde_json::Value::as_str)
}

fn fingerprints() -> &'static serde_json::Map<String, serde_json::Value> {
    use std::sync::OnceLock;
    static PARSED: OnceLock<serde_json::Map<String, serde_json::Value>> = OnceLock::new();
    PARSED.get_or_init(|| {
        serde_json::from_str(FINGERPRINTS_JSON)
            .expect("the generated workflow fingerprints are a JSON object")
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The placeholder must survive in the artifact, or every substitution
    /// silently produces DDL bound to a schema nobody asked for.
    #[test]
    fn the_template_still_carries_the_placeholder() {
        assert!(POSTGRES_TEMPLATE.contains(SCHEMA_PLACEHOLDER));
        assert!(
            !postgres_sql("journal_schema").contains(SCHEMA_PLACEHOLDER),
            "binding must leave no unbound placeholder behind"
        );
    }

    /// The `SQLite` artifact has no schema to bind, so it must not carry the
    /// `PostgreSQL` placeholder at all.
    #[test]
    fn the_sqlite_artifact_needs_no_binding() {
        assert!(!SQLITE_SQL.contains(SCHEMA_PLACEHOLDER));
    }

    /// The substitution is the one place a caller-supplied schema name reaches
    /// rendered DDL, so a quote in the name must not end the identifier.
    #[test]
    fn substitution_matches_the_orm_quoting_rule() {
        assert_eq!(quote_ident("plain"), "\"plain\"");
        assert_eq!(quote_ident("a\"b"), "\"a\"\"b\"");
        let bound = postgres_sql("a\"; DROP SCHEMA public; --");
        assert!(bound.contains(r#""a""; DROP SCHEMA public; --""#), "{bound}");
    }

    /// Both dialects are generated, and the stamp the artifact writes must be
    /// the fingerprint a host will demand. A generator that emitted one and
    /// stamped the other would refuse every journal it just installed.
    #[test]
    fn every_dialect_stamps_the_fingerprint_it_publishes() {
        for (dialect, sql) in [(POSTGRES, POSTGRES_TEMPLATE), (SQLITE, SQLITE_SQL)] {
            let expected =
                fingerprint(dialect).unwrap_or_else(|| panic!("no fingerprint for {dialect}"));
            assert_eq!(expected.len(), 64, "{dialect} fingerprint is not sha256 hex");
            assert!(
                sql.contains(expected),
                "the {dialect} artifact does not stamp its own fingerprint"
            );
        }
        assert!(fingerprint("mysql").is_none());
    }

    /// The stamp names are what an installer and a host agree on out of band;
    /// the artifact has to actually create and write that row.
    #[test]
    fn the_artifact_creates_the_stamp_the_constants_name() {
        for sql in [POSTGRES_TEMPLATE, SQLITE_SQL] {
            assert!(sql.contains(STAMP_TABLE), "{sql}");
            assert!(sql.contains(&format!("'{STAMP_ROW_ID}'")), "{sql}");
        }
    }
}
