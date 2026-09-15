//! The platform schema bundle: what a platform service sends the migration
//! service to install or upgrade a schema it owns inside a creator database.
//!
//! # It says nothing about what the schema is for
//!
//! That is the whole point. The migration service owns privileged DDL against a
//! creator database; it does not own anyone else's domain. A bundle carries the
//! DDL, the policy it declares, where its stamp lives and which version it is,
//! and the service reasons about exactly those. A second platform-owned schema
//! reuses this unchanged, which is the test of whether the boundary is real.
//!
//! # It is addressed by SCHEMA, not by app
//!
//! A creator database holds one journal for every app inside it, so the schema
//! is not derivable from an app id and the bundle never carries one. The stamp
//! is likewise ONE row per schema: installing or upgrading moves every app in
//! that database at once.

use serde::{Deserialize, Serialize};

/// The audience a bundle request is addressed to.
pub const MIGRATE_AUDIENCE: &str = "spiffe://zeroship.ai/svc/migrate-server";

/// Where the installed version and fingerprint are recorded.
///
/// The service reads and writes exactly this row and never interprets it. The
/// table is created by the bundle's own first version, so its absence is how
/// "not installed" is recognised.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct SchemaStamp {
    /// Unqualified table name inside the target schema.
    pub table: String,
    /// The `id` of the one row this bundle owns.
    pub row_id: String,
}

/// One step of the ordered series: the DDL that brings the previous version to
/// this one. Version 1 installs from nothing.
///
/// Carries NO stamp write. The service records the stamp once, after every step
/// it applied has committed.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct SchemaBundleVersion {
    /// Where this step lands in the series.
    pub version: u32,
    /// The step's DDL, already bound to [`SchemaBundle::schema`].
    pub sql: String,
}

/// A complete bundle.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct SchemaBundle {
    /// An opaque label for logs and diagnostics. The service never branches on
    /// it; it exists so an operator reading a refusal knows which bundle failed.
    pub bundle: String,
    /// The physical schema to install into.
    pub schema: String,
    /// Which dialect the SQL is written for. The migration service is
    /// PostgreSQL-only and refuses anything else.
    pub dialect: String,
    /// The version the series ends at, and the version the stamp will carry.
    pub version: u32,
    /// The fingerprint a host will compare an installed journal against.
    pub fingerprint: String,
    /// Where to read and write the stamp.
    pub stamp: SchemaStamp,
    /// The policy charter the bundle declares, as TOML. Composed against the
    /// service's own bundle ceiling with escalation-reject, exactly as a creator
    /// draft is: policy arrives in the artifact, never from the database.
    pub policy: String,
    /// The ordered series, from version 1 to [`SchemaBundle::version`].
    pub versions: Vec<SchemaBundleVersion>,
}

/// Ask the owner of a platform schema to bring one creator database's copy of it
/// to the current version.
///
/// The request names a SCHEMA rather than an app for the same reason a bundle
/// does: a creator database holds the schemas of every app inside it, and one
/// stamp covers them all.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct EnsureJournal {
    /// The physical schema whose journal must be current.
    pub schema: String,
}

/// What applying a bundle did.
#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum SchemaBundleAction {
    /// There was no stamp; the whole series ran.
    Installed,
    /// The stamp already named this version and its fingerprint matched.
    Unchanged,
    /// The stamp named an earlier version; the steps between ran.
    Upgraded,
}

/// The service's answer to an applied bundle.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct SchemaBundleOutcome {
    /// The schema that now carries the stamp.
    pub schema: String,
    /// Echoed from the request, so a log line names the bundle.
    pub bundle: String,
    /// The version the stamp now carries.
    pub version: u32,
    /// Which branch ran.
    pub action: SchemaBundleAction,
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The wire shape is a contract between two services that do not share a
    /// type at the HTTP boundary, so a round trip is what pins the field names.
    #[test]
    fn a_bundle_round_trips_through_its_wire_form() {
        let bundle = SchemaBundle {
            bundle: "workflow_journal".into(),
            schema: "customer".into(),
            dialect: "postgres".into(),
            version: 2,
            fingerprint: "a".repeat(64),
            stamp: SchemaStamp {
                table: "__zeroship_workflow_schema_version".into(),
                row_id: "workflow".into(),
            },
            policy: "policy_version = 1\n".into(),
            versions: vec![
                SchemaBundleVersion {
                    version: 1,
                    sql: "CREATE TABLE a ()".into(),
                },
                SchemaBundleVersion {
                    version: 2,
                    sql: "CREATE TABLE b ()".into(),
                },
            ],
        };
        let json = serde_json::to_string(&bundle).unwrap();
        assert_eq!(
            serde_json::from_str::<SchemaBundle>(&json).unwrap(),
            bundle,
            "the bundle wire form is not stable through a round trip"
        );
        assert!(
            !json.contains("app_id"),
            "a bundle must carry no app identity: {json}"
        );
    }

    /// The audience is written out so a caller can address the service without
    /// building an issuer, and pinned here so the two cannot drift.
    #[test]
    fn the_audience_is_the_migration_service_issuer() {
        assert_eq!(
            MIGRATE_AUDIENCE,
            crate::service_peers::service_issuer(crate::service_peers::MIGRATE_SERVICE_NAME)
                .expect("the migration service name parses as an issuer")
                .as_str()
        );
    }

    /// The action is part of the answer an operator reads, so its spellings are
    /// a contract too.
    #[test]
    fn the_outcome_action_renders_as_snake_case() {
        for (action, expected) in [
            (SchemaBundleAction::Installed, "\"installed\""),
            (SchemaBundleAction::Unchanged, "\"unchanged\""),
            (SchemaBundleAction::Upgraded, "\"upgraded\""),
        ] {
            assert_eq!(serde_json::to_string(&action).unwrap(), expected);
        }
    }
}
